/**
 * `platform secret`: an app's values in the secret service, each written once for every node the
 * app is placed on, which host writes into its environment. A value is read from standard input,
 * never from an argument, and never printed back. See spec/architecture/secrets.md and RFC 231yq.
 */
import { parseArgs } from 'node:util';
import { fail } from './failure.ts';

export const TOKEN = 'SECRETS_TOKEN';
/** The secret service's port on each core's tailnet address. */
export const PORT = 12014;
/** Who a change is recorded as made by. */
const BY = 'operator';
const TIMEOUT_MS = 10_000;
const VERBS = ['list', 'set', 'delete'] as const;
type Verb = (typeof VERBS)[number];

const USAGE =
	'`mise run platform secret list [app] [--scope <scope>] [--json]`, ' +
	'`secret set <app> <NAME> [--config] [--scope <scope>] [--dry-run]` with the value on standard ' +
	'input, `secret delete <app> <NAME> [--scope <scope>] [--dry-run]`';

/** What the subcommand reaches outside itself, replaced whole by the test. */
export interface SecretWorld {
	/** The secret service's cores, by name, in order. */
	placements(): string[];
	/** A core's address on the tailnet. */
	address(node: string): string;
	port: number;
	token(): string | undefined;
	/** The value: typed in unechoed on a terminal, or one line of a pipe. */
	typedIn(prompt: string): Promise<string>;
	print(text: string): void;
}

/** The `response` envelope, a refusal's as much as a success's. */
interface Envelope {
	status?: string;
	data?: unknown;
	code?: string;
	message?: string;
}

/** A value as a list or a read answers it: a `config`'s value alone, never a `secret`'s. */
interface Item {
	scope: string;
	app: string;
	name: string;
	class: 'config' | 'secret';
	revision: number;
	changed: string;
	value?: string;
}

/** One DNS label, as every app's and every scope's name is. */
const LABEL = /^(?!-)[a-z0-9-]{1,63}(?<!-)$/;
/** A variable's name, as host writes it into the app's environment. */
const VARIABLE = /^[A-Z_][A-Z0-9_]{0,127}$/;

/** The envelope of `path` at `address`; undefined when it did not answer at all. */
async function call(
	world: SecretWorld,
	address: string,
	method: string,
	path: string,
	secret: string,
	body?: unknown,
): Promise<Envelope | undefined> {
	const headers: Record<string, string> = {
		'Content-Type': 'application/json',
		Authorization: `Bearer ${secret}`,
	};
	let answer: Response;
	try {
		answer = await fetch(`http://${address}:${world.port}${path}`, {
			method,
			headers,
			body: body === undefined ? undefined : JSON.stringify(body),
			signal: AbortSignal.timeout(TIMEOUT_MS),
		});
	} catch {
		return undefined;
	}
	const text = await answer.text();
	try {
		return JSON.parse(text) as Envelope;
	} catch {
		if (answer.ok) return undefined;
		fail(`${method} ${path}: refused: ${answer.status} ${text.trim()}`);
	}
}

/**
 * The envelope of the first core to answer, in order. A write sent again to the next core after
 * one gave no answer is held to the same revision, so it is never applied twice.
 */
async function ask(
	world: SecretWorld,
	method: string,
	path: string,
	body?: unknown,
): Promise<Envelope> {
	const secret = world.token() || fail(`secrets.json has no ${TOKEN}; \`seal token\` makes it`);
	for (const node of world.placements()) {
		const answer = await call(world, world.address(node), method, path, secret, body);
		if (answer !== undefined) return answer;
	}
	fail(`no core of the secret service answered ${method} ${path}`);
}

/** The `data` of a success; a refusal fails, with its code and message. */
function dataOf(answer: Envelope, what: string): unknown {
	if (answer.status === 'success') return answer.data;
	fail(`${what}: refused ${answer.code}: ${answer.message}`);
}

const isVerb = (word: string | undefined): word is Verb => VERBS.includes(word as Verb);

interface Given {
	app: string | undefined;
	name: string | undefined;
	scope: string | undefined;
	config: boolean;
	json: boolean;
	dry: boolean;
}

/** What the verb is given, each name held to its shape before anything is sent. */
function given(verb: Verb, args: readonly string[]): Given {
	let parsed;
	try {
		parsed = parseArgs({
			args: [...args],
			allowPositionals: true,
			options: {
				scope: { type: 'string' },
				config: { type: 'boolean', default: false },
				json: { type: 'boolean', default: false },
				'dry-run': { type: 'boolean', default: false },
			},
		});
	} catch (error) {
		fail(`${(error as Error).message}; ${USAGE}`);
	}
	const { values, positionals } = parsed;
	const [app, name, ...more] = positionals;
	const named = verb !== 'list';
	if (more.length > 0 || (named && name === undefined) || (!named && name !== undefined)) {
		fail(USAGE);
	}
	if (values.config && verb !== 'set') fail('--config is given to `set` alone');
	if ((values.json && verb !== 'list') || (values['dry-run'] && verb === 'list')) fail(USAGE);
	if (app !== undefined && !LABEL.test(app)) fail(`${app} is no app's name: one DNS label`);
	if (name !== undefined && !VARIABLE.test(name)) {
		fail(`${name} is no variable's name: capitals, digits and underscores, not starting with a digit`);
	}
	const scope = values.scope;
	if (scope !== undefined && !LABEL.test(scope)) fail(`${scope} is no scope: one DNS label`);
	return { app, name, scope, config: values.config, json: values.json, dry: values['dry-run'] };
}

/** The path of one value, with its scope when one is given. */
function pathOf(app: string, name: string, scope: string | undefined): string {
	const query = scope === undefined ? '' : `?scope=${scope}`;
	return `/v1/secrets/${app}/${name}${query}`;
}

/** The revision of the value at `path`, 0 for one that does not exist. */
async function revisionOf(world: SecretWorld, path: string, what: string): Promise<number> {
	const answer = await ask(world, 'GET', path);
	if (answer.status !== 'success' && answer.code === 'no_such_resource') return 0;
	return (dataOf(answer, what) as Item).revision;
}

/** Every value as a table: a `config`'s value shown, a `secret` by its name alone. */
export function table(items: readonly Item[]): string {
	const rows = [['scope', 'app', 'name', 'class', 'revision', 'changed', 'value']];
	for (const item of items) {
		const value = item.class === 'config' ? (item.value ?? '') : '-';
		rows.push([item.scope, item.app, item.name, item.class, String(item.revision), item.changed, value]);
	}
	const widths = rows[0]!.map((_, at) => Math.max(...rows.map((row) => row[at]!.length)));
	return rows
		.map((row) =>
			row
				.map((text, at) => text.padEnd(widths[at]!))
				.join('  ')
				.trimEnd(),
		)
		.join('\n');
}

/** The subcommand: what it prints goes through `world`, and a refusal is thrown as a `Failure`. */
export async function runSecret(world: SecretWorld, args: readonly string[]): Promise<void> {
	const [verb, ...rest] = args;
	if (!isVerb(verb)) fail(USAGE);
	const options = given(verb, rest);
	if (verb === 'list') {
		const listed = dataOf(await ask(world, 'GET', '/v1/secrets'), 'GET /v1/secrets');
		const items = (listed as { items: Item[] }).items.filter(
			(item) =>
				(options.app === undefined || item.app === options.app) &&
				(options.scope === undefined || item.scope === options.scope),
		);
		world.print(options.json ? JSON.stringify(items, null, 2) : table(items));
		return;
	}
	const { app, name, scope } = options as Given & { app: string; name: string };
	const path = pathOf(app, name, scope);
	const what = `${scope ?? 'platform'}/${app}/${name}`;
	const revision = await revisionOf(world, path, what);
	if (verb === 'delete') {
		if (revision === 0) fail(`${what}: no such value`);
		const body = { by: BY, revision };
		if (options.dry) {
			world.print(`platform: would DELETE ${path} at revision ${revision}, as Bearer <redacted>`);
			return;
		}
		const done = dataOf(await ask(world, 'DELETE', path, body), what) as { revision: number };
		world.print(`platform: ${what}: removed, revision ${done.revision}`);
		return;
	}
	const kind = options.config ? 'config' : 'secret';
	if (options.dry) {
		world.print(
			`platform: would PUT ${path} as ${kind} at revision ${revision}, the value read from` +
				' standard input, as Bearer <redacted>',
		);
		return;
	}
	const value = await world.typedIn(`${what} (${kind}): `);
	if (!value) fail(`${what}: no value given on standard input`);
	const body = { by: BY, revision, value, class: kind };
	const done = dataOf(await ask(world, 'PUT', path, body), what) as { revision?: number };
	world.print(
		done.revision === undefined
			? `platform: ${what}: unchanged`
			: `platform: ${what}: written as ${kind}, revision ${done.revision}`,
	);
}
