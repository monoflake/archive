#!/usr/bin/env node
//MISE description="Control in its own Cloudflare account: `control deploy|token|import [--dry-run]`"
/**
 * The one way `apps/control/control` is deployed, given its tokens, and filled from the old
 * registry. Its account is not the platform's, so wrangler is handed control's token and the one
 * account it reaches, in its own environment alone, every other Cloudflare variable left out. No
 * token and no account id is printed. See RFC o7eir.
 */
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { parseArgs } from 'node:util';
import { parse } from 'smol-toml';
import * as v from 'valibot';
import { PORT, address, placements } from '../../apps/data/registry/scripts/platform.ts';
import { URLS } from '../../libs/sdk/src/index.ts';
import { ROOT } from './workspace.ts';

const APP = join(ROOT, 'apps/control/control');
const WORKER = 'control';
const READ = 'CONTROL_READ_TOKEN';
const IMPORT = 'CONTROL_IMPORT_TOKEN';
const NODES = join(ROOT, '../infra/nodes/nodes.toml');
/** What control shows a node as, from nodes.toml -- infra's spec/architecture/nodes.md. */
const SHOWN = ['city', 'country', 'continent'] as const;
/** What the old registry keeps that control does not: the display it replaces. */
const DROPPED = ['place'];
const TIMEOUT_MS = 10_000;
const STEPS = [
	['wrangler', 'd1', 'migrations', 'apply', 'control', '--remote'],
	// `--env=` names the top level, as every deploy here does; see mise.toml, `deploy-cdn`.
	['wrangler', 'deploy', '--env='],
];

const Accounts = v.object({
	success: v.literal(true),
	result: v.array(v.object({ id: v.pipe(v.string(), v.nonEmpty()) })),
});

const Item = v.object({
	id: v.string(),
	name: v.nullable(v.string()),
	revision: v.number(),
	spec: v.record(v.string(), v.unknown()),
});
const Listed = v.object({
	status: v.literal('success'),
	data: v.object({ items: v.array(Item), revision: v.number() }),
});
type Item = v.InferOutput<typeof Item>;

function fail(message: string): never {
	console.error(`control: ${message}`);
	process.exit(1);
}

/** The one account `token` reaches, by asking Cloudflare. */
async function account(token: string): Promise<string> {
	const response = await fetch(`${URLS.external.cloudflare.api}/accounts`, {
		headers: { Authorization: `Bearer ${token}` },
	});
	const parsed = v.safeParse(Accounts, await response.json().catch(() => undefined));
	if (!response.ok || !parsed.success) {
		fail(`Cloudflare did not list the token's accounts (${response.status})`);
	}
	const [only, ...more] = parsed.output.result;
	if (!only || more.length) {
		fail(`CLOUDFLARE_CONTROL_TOKEN reaches ${parsed.output.result.length} accounts, not one`);
	}
	return only.id;
}

/** This environment without anything that names a Cloudflare account or credential. */
function scrubbed(): NodeJS.ProcessEnv {
	return Object.fromEntries(
		Object.entries(process.env).filter(([name]) => !/^(CLOUDFLARE|CF)_/.test(name)),
	);
}

/** What wrangler runs in: control's token and account, and nothing else of Cloudflare's. */
async function wranglerEnv(): Promise<{ env: NodeJS.ProcessEnv; account: string; token: string }> {
	const token = process.env.CLOUDFLARE_CONTROL_TOKEN;
	if (!token) fail('CLOUDFLARE_CONTROL_TOKEN is not set');
	const id = await account(token);
	return {
		env: { ...scrubbed(), CLOUDFLARE_API_TOKEN: token, CLOUDFLARE_ACCOUNT_ID: id },
		account: id,
		token,
	};
}

/** Runs `command` with `stdin`, failing by `what` without echoing what it was given. */
function run(
	command: string[],
	what: string,
	options: { cwd: string; env?: NodeJS.ProcessEnv; input?: string },
) {
	const [program = '', ...args] = command;
	const ran = spawnSync(program, args, {
		cwd: options.cwd,
		env: options.env ?? process.env,
		input: options.input,
		stdio: [options.input === undefined ? 'inherit' : 'pipe', 'inherit', 'inherit'],
	});
	if (ran.status !== 0) fail(`${what} failed`);
}

async function deploy(dry: boolean): Promise<void> {
	if (dry) {
		const token = process.env.CLOUDFLARE_CONTROL_TOKEN ? 'set' : 'not set, so a run would stop';
		console.log(
			`would ask Cloudflare for the one account CLOUDFLARE_CONTROL_TOKEN reaches (${token})`,
		);
		for (const step of STEPS) console.log(`would run, in ${APP}: pnpm exec ${step.join(' ')}`);
		return;
	}
	const { env } = await wranglerEnv();
	for (const step of STEPS)
		run(['pnpm', 'exec', ...step], step.slice(0, 2).join(' '), { cwd: APP, env });
	console.log('control: deployed');
}

/**
 * Both bearers drawn anew, each put on the Worker and into secrets.json on stdin alone; the old
 * ones stop working as the Worker takes the new. A Worker not there yet is refused, since
 * `wrangler secret put` would make an empty one of that name.
 */
async function token(dry: boolean): Promise<void> {
	if (dry) {
		for (const name of [READ, IMPORT]) {
			console.log(`would draw ${name}, put it on the Worker ${WORKER}, and set it in secrets.json`);
		}
		return;
	}
	const { env, account: id, token: credential } = await wranglerEnv();
	const script = await fetch(
		`${URLS.external.cloudflare.api}/accounts/${id}/workers/scripts/${WORKER}/settings`,
		{ headers: { Authorization: `Bearer ${credential}` } },
	);
	if (!script.ok)
		fail(`the Worker ${WORKER} is not deployed (${script.status}); run \`control deploy\` first`);
	for (const name of [READ, IMPORT]) {
		const value = randomBytes(32).toString('base64url');
		run(['pnpm', 'exec', 'wrangler', 'secret', 'put', name, '--name', WORKER], `putting ${name}`, {
			cwd: APP,
			env,
			input: value,
		});
		run(['sops', 'set', '--value-stdin', 'secrets.json', `["${name}"]`], `setting ${name}`, {
			cwd: ROOT,
			input: JSON.stringify(value),
		});
		console.log(`control: ${name} drawn, put on ${WORKER} and set in secrets.json`);
	}
}

/** Every resource of `plural` the old registry holds, from the first core to answer. */
async function read(plural: string): Promise<{ items: Item[]; revision: number }> {
	for (const core of placements()) {
		let body: unknown;
		try {
			const answer = await fetch(`http://${address(core)}:${PORT}/v1/${plural}`, {
				signal: AbortSignal.timeout(TIMEOUT_MS),
			});
			body = await answer.json();
		} catch {
			continue;
		}
		const parsed = v.safeParse(Listed, body);
		if (!parsed.success) fail(`the registry on ${core} answered /v1/${plural} with no list`);
		return parsed.output.data;
	}
	fail(`no registry core answered /v1/${plural}`);
}

/** Each node's table in nodes.toml, by its name. */
function tables(): Map<string, Record<string, unknown>> {
	const declared = parse(readFileSync(NODES, 'utf8')) as Record<string, unknown>;
	return new Map(
		Object.entries(declared).filter(
			(entry): entry is [string, Record<string, unknown>] =>
				typeof entry[1] === 'object' && entry[1] !== null && !Array.isArray(entry[1]),
		),
	);
}

/**
 * The import's body: every resource as the old registry holds it, a node with no name and with
 * what control shows it as, an app placed on node ids. Exported for the test.
 */
export function body(
	nodes: { items: Item[]; revision: number },
	apps: { items: Item[]; revision: number },
	accounts: { items: Item[]; revision: number },
	declared: Map<string, Record<string, unknown>>,
) {
	const ids = new Map<string, string>();
	const imported = nodes.items.map((node) => {
		if (!node.name) fail(`the registry's node ${node.id} has no name to find in nodes.toml`);
		ids.set(node.name, node.id);
		const table = declared.get(node.name) ?? fail(`nodes.toml has no [${node.name}]`);
		const spec = Object.fromEntries(
			Object.entries(node.spec).filter(([key]) => !DROPPED.includes(key)),
		);
		for (const key of SHOWN) {
			const value = table[key];
			if (typeof value !== 'string') fail(`nodes.toml's [${node.name}] has no ${key}`);
			spec[key] = value;
		}
		return { id: node.id, name: null, revision: node.revision, spec };
	});
	const placed = apps.items.map((app) => {
		const names = app.spec.placements;
		if (!Array.isArray(names)) fail(`the app ${app.name} has no placements`);
		const placements = names.map((name) =>
			typeof name === 'string'
				? (ids.get(name) ?? fail(`${app.name} is placed on ${name}, no node`))
				: fail(`${app.name} has a placement that is not a name`),
		);
		return {
			id: app.id,
			name: app.name,
			revision: app.revision,
			spec: { ...app.spec, placements },
		};
	});
	return {
		revision: Math.max(nodes.revision, apps.revision, accounts.revision),
		nodes: imported,
		apps: placed,
		accounts: accounts.items.map(({ id, revision, spec }) => ({ id, name: null, revision, spec })),
	};
}

/** The old registry copied into control whole, as many times as it is run. */
async function load(dry: boolean): Promise<void> {
	const [nodes, apps, accounts] = [await read('nodes'), await read('apps'), await read('accounts')];
	const sent = body(nodes, apps, accounts, tables());
	const counts = `${sent.nodes.length} nodes, ${sent.apps.length} apps, ${sent.accounts.length} accounts at revision ${sent.revision}`;
	if (dry) {
		console.log(`would import ${counts} into ${URLS.control}`);
		return;
	}
	const token = process.env[IMPORT] ?? fail(`${IMPORT} is not set; \`control token\` draws it`);
	const answer = await fetch(`${URLS.control}/v1/import`, {
		method: 'PUT',
		headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
		body: JSON.stringify(sent),
		signal: AbortSignal.timeout(TIMEOUT_MS),
	});
	const text = await answer.text();
	if (!answer.ok) fail(`control refused the import: ${answer.status} ${text.trim()}`);
	console.log(`control: imported ${counts}`);
}

if (import.meta.main) {
	const { values, positionals } = parseArgs({
		options: { 'dry-run': { type: 'boolean', short: 'n' } },
		allowPositionals: true,
	});
	const dry = values['dry-run'] ?? false;
	const [verb] = positionals;
	if (verb === 'deploy') await deploy(dry);
	else if (verb === 'token') await token(dry);
	else if (verb === 'import') await load(dry);
	else fail('usage: control deploy|token|import [--dry-run]');
}
