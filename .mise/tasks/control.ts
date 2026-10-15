#!/usr/bin/env node
//MISE description="Control in its own Cloudflare account: `control deploy|token|hook|writers|import [--dry-run]`"
/**
 * The one way `apps/control/control` is deployed, given its tokens and GitHub's webhook secret, its
 * writers given theirs, and filled from the old registry. Its account is not the platform's, so
 * wrangler is handed control's token and the one account it reaches, in its own environment
 * alone, every other Cloudflare variable left out. No token and no account id is printed. See RFC
 * o7eir and spec/architecture/control.md.
 */
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { parseArgs } from 'node:util';
import { parse } from 'smol-toml';
import * as v from 'valibot';
import {
	Failure,
	address,
	placedOn,
	placements,
} from '../../apps/data/registry/scripts/platform.ts';
import { accountVariables } from '../../apps/system/deployer/src/config.ts';
import { URLS } from '../../libs/sdk/src/index.ts';
import { ROOT } from './workspace.ts';

const APP = join(ROOT, 'apps/control/control');
const WORKER = 'control';
const READ = 'CONTROL_READ_TOKEN';
const WRITE = 'CONTROL_WRITE_TOKEN';
const IMPORT = 'CONTROL_IMPORT_TOKEN';
/** What the hook scope checks GitHub's deliveries against; the repositories' webhooks keep it. */
const WEBHOOK = 'WEBHOOK_SECRET';
/** The tokens infra holds too: hosts read control, and an operator's deploy by hand writes it. */
const SHARED = new Set([READ, WRITE]);
/** The old registry's port on each of its cores, which `import` reads until the cut. */
const REGISTRY_PORT = 12013;
const INFRA = join(ROOT, '../infra');
const NODES = join(INFRA, 'nodes/nodes.toml');
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

/**
 * Whether the Worker `worker` is deployed in the account `id`, its status when it is not: a secret
 * is put only on one that is, since `wrangler secret put` without one makes an empty Worker.
 */
async function deployed(worker: string, id: string, credential: string): Promise<true | number> {
	const script = await fetch(
		`${URLS.external.cloudflare.api}/accounts/${id}/workers/scripts/${worker}/settings`,
		{ headers: { Authorization: `Bearer ${credential}` } },
	);
	return script.ok || script.status;
}

/** Fails unless control is deployed in its account `id`. */
async function controlDeployed(id: string, credential: string): Promise<void> {
	const status = await deployed(WORKER, id, credential);
	if (status !== true)
		fail(`the Worker ${WORKER} is not deployed (${status}); run \`control deploy\` first`);
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
 * Every bearer drawn anew, each put on the Worker and into secrets.json on stdin alone, the read
 * and write ones into infra's too, which hosts read control with and a deploy by hand writes it
 * with; the old ones stop working as the Worker takes the new, so `mise run node` in infra,
 * `seal token` and `control writers` follow. A Worker not there yet is refused, since
 * `wrangler secret put` would make an empty one of that name.
 */
async function token(dry: boolean): Promise<void> {
	if (dry) {
		for (const name of [READ, WRITE, IMPORT]) {
			const where = SHARED.has(name) ? 'secrets.json here and in infra' : 'secrets.json';
			console.log(`would draw ${name}, put it on the Worker ${WORKER}, and set it in ${where}`);
		}
		console.log('then `mise run node` in infra, `seal token` and `control writers` give them out');
		return;
	}
	const { env, account: id, token: credential } = await wranglerEnv();
	await controlDeployed(id, credential);
	for (const name of [READ, WRITE, IMPORT]) {
		const value = randomBytes(32).toString('base64url');
		run(['pnpm', 'exec', 'wrangler', 'secret', 'put', name, '--name', WORKER], `putting ${name}`, {
			cwd: APP,
			env,
			input: value,
		});
		for (const cwd of SHARED.has(name) ? [ROOT, INFRA] : [ROOT]) {
			run(['sops', 'set', '--value-stdin', 'secrets.json', `["${name}"]`], `setting ${name}`, {
				cwd,
				input: JSON.stringify(value),
			});
		}
		console.log(`control: ${name} drawn, put on ${WORKER} and set in secrets.json`);
	}
	console.log('control: then `mise run node` in infra, `seal token` and `control writers`');
}

/**
 * `WEBHOOK_SECRET` from secrets.json put on control, on stdin, for its hook scope. Its own verb
 * rather than part of `token`: the secret is copied, never drawn, so putting it rotates nothing.
 */
async function hook(dry: boolean): Promise<void> {
	const secret = process.env[WEBHOOK];
	if (dry) {
		const set = secret ? 'set' : 'not set, so a run would stop';
		console.log(`would check the Worker ${WORKER} is there, then put ${WEBHOOK} (${set}) on it`);
		return;
	}
	if (!secret) fail(`secrets.json has no ${WEBHOOK}`);
	const { env, account: id, token: credential } = await wranglerEnv();
	await controlDeployed(id, credential);
	run(
		['pnpm', 'exec', 'wrangler', 'secret', 'put', WEBHOOK, '--name', WORKER],
		`putting ${WEBHOOK}`,
		{
			cwd: APP,
			env,
			input: secret,
		},
	);
	console.log(`control: ${WEBHOOK} put on ${WORKER}, read by its next request`);
}

/**
 * The app that writes control while it runs: the deployer, recording each app's release -- RFC
 * jln34. It is given where control is and the write token, and with them every source host deploys
 * from added to its own `DEPLOY_SOURCES`, so no app host runs is left without a release;
 * `GITHUB_ACTIONS_TOKEN`, which reads those sources' runs; and the bucket CI leaves each run's
 * artifacts in, RFC q28g5 and 7cwmp. An owner's own GitHub token is the author's, kept apart and
 * never written here -- RFC 024i3.
 */
const WRITER = 'deployer';
const GITHUB = 'GITHUB_ACTIONS_TOKEN';
const SOURCES = 'DEPLOY_SOURCES';
const ARTIFACTS: Record<string, string> = {
	ARTIFACTS_ENDPOINT: URLS.external.artifacts,
	ARTIFACTS_REGION: 'auto',
	ARTIFACTS_BUCKET: 'artifact',
};
const ARTIFACTS_KEY = ['ARTIFACTS_READ_KEY_ID', 'ARTIFACTS_READ_SECRET'];
/**
 * The console, which the deployer deploys into control's account and which reaches control by its
 * service binding, holding no control token: only the read token every node's host takes, from
 * infra's secrets.json. See spec/architecture/control.md.
 */
const CONSOLE = 'console';
const HOST_READ = 'HOST_READ_TOKEN';
/** Control's account, as the deployer names it: `WORKER_ACCOUNTS` sends the console there. */
const ACCOUNT = 'monoflake';
const [ACCOUNT_ID] = accountVariables(ACCOUNT);
const ACCOUNTS = { WORKER_ACCOUNTS: `${CONSOLE}=${ACCOUNT}` };
const SSH = ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10'];

/**
 * Run on a node with KEY=VALUE lines on stdin and the app's directory and the file's name as
 * arguments: each key set, every other line kept, the file root's alone; prints current or
 * changed. The database task's `UPSERT`, which every other env write here goes through.
 */
export const UPSERT = `set -eu
umask 077
dir=$1
file=$dir/$2
mountpoint -q /data || { echo "/data is not mounted" >&2; exit 1; }
mkdir -p /data/apps
[ -d "$dir" ] || btrfs subvolume create "$dir" >/dev/null
chmod 700 "$dir"
cp -p "$file" "$file.new" 2>/dev/null || : > "$file.new"
while IFS= read -r pair; do
	[ -n "$pair" ] || continue
	K=\${pair%%=*} V=\${pair#*=} awk 'BEGIN { k = ENVIRON["K"]; v = ENVIRON["V"] }
		index($0, k "=") == 1 { if (!d) print k "=" v; d = 1; next }
		{ print }
		END { if (!d) print k "=" v }' "$file.new" > "$file.tmp"
	mv "$file.tmp" "$file.new"
done
if [ -f "$file" ] && cmp -s "$file.new" "$file"; then
	rm -f "$file.new"
	echo current
else
	chown root:root "$file.new"
	chmod 600 "$file.new"
	mv "$file.new" "$file"
	echo changed
fi
`;

/** `text` quoted for a POSIX shell. */
const quoted = (text: string) => `'${text.replaceAll("'", `'\\''`)}'`;

/** What `command` printed on `node`, given `input`; undefined when it failed. */
function ssh(node: string, command: string, input = ''): string | undefined {
	const ran = spawnSync('ssh', [...SSH, `root@${node}`, command], {
		input,
		encoding: 'utf8',
		stdio: ['pipe', 'pipe', 'inherit'],
	});
	return ran.status === 0 ? ran.stdout.trim() : undefined;
}

/** `pairs` set in /data/apps/<app>/<file> on `node`, every other line kept; a failure stops. */
function upsert(node: string, app: string, file: string, pairs: Record<string, string>): void {
	const command = `sh -c ${quoted(UPSERT)} sh ${quoted(`/data/apps/${app}`)} ${quoted(file)}`;
	const lines = Object.entries(pairs).map(([key, value]) => `${key}=${value}\n`);
	const answer = ssh(node, command, lines.join(''));
	if (answer === undefined) fail(`${node}: writing /data/apps/${app}/${file} failed`);
	console.log(`control: ${node}: /data/apps/${app}/${file} ${answer}`);
}

/** `key` as /data/apps/<app>/config.env on `node` sets it, '' when it sets none. */
function configured(node: string, app: string, key: string): string {
	const path = quoted(`/data/apps/${app}/config.env`);
	const answer = ssh(node, `if [ -f ${path} ]; then sed -n 's/^${key}=//p' ${path}; fi`);
	return answer ?? fail(`${node}: reading ${key} of ${app} failed`);
}

/**
 * Each `owner/repo` that `DEPLOY_SOURCES` names in the text of infra's host_env.py, read rather
 * than run; undefined where it holds none. Exported for the test.
 */
export function deploySources(text: string): string[] | undefined {
	const found = /^DEPLOY_SOURCES\s*=\s*"([^"]*)"/m.exec(text);
	return found?.[1]?.split(/\s+/).flatMap((entry) => (entry ? [entry.split('=')[0]!] : []));
}

/** The sources `current` names, in its order, then each of `wanted` it does not, each once. */
export function widened(current: string, wanted: readonly string[]): string {
	const have = [...new Set(current.split(/\s+/).filter(Boolean))];
	return [...have, ...wanted.filter((source) => !have.includes(source))].join(' ');
}

/** Every repository host deploys from, as DEPLOY_SOURCES names them on infra's `main`. */
function hostSources(): string[] {
	const file = '.mise/tasks/host_env.py';
	const args = ['--ignore-working-copy', '-R', INFRA, 'file', 'show', '-r', 'main', file];
	const shown = spawnSync('jj', args, { cwd: INFRA, encoding: 'utf8' });
	if (shown.status !== 0) fail(`reading ${file} on infra's main failed`);
	return deploySources(shown.stdout) ?? fail(`infra's ${file} holds no DEPLOY_SOURCES string`);
}

/** `name` from infra's secrets.json, by sops on a pipe as infra's own tasks read one. */
function infraSecret(name: string): string | undefined {
	const read = spawnSync('sops', ['decrypt', '--extract', JSON.stringify([name]), 'secrets.json'], {
		cwd: INFRA,
		encoding: 'utf8',
		stdio: ['ignore', 'pipe', 'ignore'],
	});
	return read.status === 0 && read.stdout.trim() ? read.stdout.trim() : undefined;
}

/**
 * What the deployer's `secret.env` and `config.env` are given, besides the sources it already
 * names; `id` is control's account id, which sends the console there. Exported for the test.
 */
export function writerEnv(secrets: Record<string, string>, id: string) {
	return {
		secret: { ...secrets, [ACCOUNT_ID]: id },
		config: { CONTROL: URLS.control, ...ARTIFACTS, ...ACCOUNTS },
	};
}

/**
 * The deployer, on each node control places it on, given what it writes control with and control's
 * account, and the console Worker given the read token hosts take, as the old registry's writers
 * were by `registry token`. The console must be in control's account first: `wrangler secret put`
 * without one makes it.
 */
async function writers(dry: boolean): Promise<void> {
	const [reading, writing, github] = [READ, WRITE, GITHUB].map((name) => process.env[name]);
	const key = Object.fromEntries(ARTIFACTS_KEY.map((name) => [name, process.env[name] ?? '']));
	const missing = [
		...(reading ? [] : [READ]),
		...(writing ? [] : [WRITE]),
		...(github ? [] : [GITHUB]),
		...ARTIFACTS_KEY.filter((name) => !key[name]),
		...(process.env.CLOUDFLARE_CONTROL_TOKEN ? [] : ['CLOUDFLARE_CONTROL_TOKEN']),
	];
	if (missing.length > 0) {
		const said = `secrets.json has no ${missing.join(', ')}; \`control token\` draws control's`;
		if (!dry) fail(said);
		console.log(`${said} (a real run stops here)`);
	}
	const nodes = reading ? await placedOn(WRITER) : [];
	if (reading && nodes.length === 0) fail(`control places ${WRITER} on no node`);
	const sources = hostSources();
	if (dry) {
		const where = reading ? nodes.join(', ') : `the nodes control places ${WRITER} on`;
		const secrets = [WRITE, GITHUB, ...ARTIFACTS_KEY, ACCOUNT_ID].map(
			(name) => `${name}=<redacted>`,
		);
		console.log(`would ask Cloudflare for the one account CLOUDFLARE_CONTROL_TOKEN reaches`);
		console.log(`would, on ${where}, set in /data/apps/${WRITER}/secret.env: ${secrets.join(' ')}`);
		const told = Object.entries(writerEnv({}, '').config).map(
			([name, value]) => `${name}=${value}`,
		);
		console.log(`would set in /data/apps/${WRITER}/config.env: ${told.join(' ')}`);
		console.log(`would add to its ${SOURCES} each of ${sources.join(' ')} it does not name`);
		console.log(
			`would put ${HOST_READ}, read from infra's secrets.json, on the Worker ${CONSOLE} on stdin` +
				` if it is in that account, and say to run this again if not`,
		);
		return;
	}
	const hostRead = infraSecret(HOST_READ) ?? fail(`infra's secrets.json has no ${HOST_READ}`);
	const { env, account: id, token: credential } = await wranglerEnv();
	const { secret, config } = writerEnv({ [WRITE]: writing!, [GITHUB]: github!, ...key }, id);
	for (const node of nodes) {
		const current = configured(node, WRITER, SOURCES);
		upsert(node, WRITER, 'secret.env', secret);
		upsert(node, WRITER, 'config.env', { ...config, [SOURCES]: widened(current, sources) });
	}
	// The deployer puts the console in control's account only once it holds that account's id.
	const status = await deployed(CONSOLE, id, credential);
	if (status === true) {
		const put = ['pnpm', 'exec', 'wrangler', 'secret', 'put', HOST_READ, '--name', CONSOLE];
		run(put, `putting ${HOST_READ}`, { cwd: APP, env, input: hostRead });
		console.log(
			`control: ${CONSOLE}: ${HOST_READ} put as a Worker secret, read by its next request`,
		);
	} else {
		console.log(
			`control: ${CONSOLE} is not in control's account yet (${status}), so ${HOST_READ} is not` +
				` put; run \`control writers\` again once the deployer has deployed it`,
		);
	}
	console.log(
		`control: ${WRITER} reads them on its next deploy;` +
			` \`mise run //repos/infra:node redeploy <node> ${WRITER}\``,
	);
}

/** Every resource of `plural` the old registry holds, from the first core to answer. */
async function read(plural: string): Promise<{ items: Item[]; revision: number }> {
	for (const core of placements()) {
		let body: unknown;
		try {
			const answer = await fetch(`http://${address(core)}:${REGISTRY_PORT}/v1/${plural}`, {
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
	const answer = await fetch(`${URLS.control}/v1/registry/import`, {
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
	try {
		if (verb === 'deploy') await deploy(dry);
		else if (verb === 'token') await token(dry);
		else if (verb === 'hook') await hook(dry);
		else if (verb === 'writers') await writers(dry);
		else if (verb === 'import') await load(dry);
		else fail('usage: control deploy|token|hook|writers|import [--dry-run]');
	} catch (error) {
		if (!(error instanceof Failure)) throw error;
		fail(error.message);
	}
}
