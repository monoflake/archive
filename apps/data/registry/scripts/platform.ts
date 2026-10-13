/**
 * The command line, the console's other face: every kind the registry holds, read off a catalog
 * held to its generated reader, a secret typed in and never taken from an argument. See
 * spec/architecture/registry.md, "The command line is the console's other face", and RFC cpc2b.
 */
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
import { parseArgs } from 'node:util';
import { parse } from 'smol-toml';
import * as v from 'valibot';
import { Catalog, type Description, type Field } from './catalog.gen.ts';

const APP = 'platform';
/** Who a change is recorded as made by. */
const BY = 'operator';
/** What a dry run shows in place of every secret value, none of which it asks for. */
const HIDDEN = '<hidden>';
/** A resource's id as a list shows it: its first eight digits -- web's spec/console/design.md. */
const SHORT = 8;
const TOKEN = 'REGISTRY_TOKEN';
const PORT = 12013;
const TIMEOUT_MS = 10_000;
const VERBS = ['list', 'get', 'create', 'patch'] as const;
type Verb = (typeof VERBS)[number];
/** The options every write takes, which no item's option may be named. */
const WRITE_OPTIONS = ['set', 'json', 'unset', 'dry-run', 'help'];

/** A failure the task reports as one line and exits 1 on. */
export class Failure extends Error {}

function fail(message: string): never {
	throw new Failure(message);
}

/** A resource as `/v1` answers it -- RFC cpc2b. */
interface Resource {
	kind: string;
	id: string;
	name: string;
	revision: number;
	spec?: Record<string, unknown>;
}

/** The `response` envelope, a refusal's as much as a success's. */
interface Envelope {
	status?: string;
	data?: unknown;
	code?: string;
	message?: string;
}

/** What the task reaches outside itself, replaced whole by the test. */
export interface World {
	/** The registry's cores, by name, in order. */
	placements(): string[];
	/** A core's address on the tailnet. */
	address(node: string): string;
	port: number;
	token(): string | undefined;
	/** A secret typed in on the terminal, never echoed. */
	typedIn(prompt: string): Promise<string>;
	print(text: string): void;
}

/** 16 lowercase hexadecimal digits: what the registry reads as an id, never a name -- RFC 2kuvi. */
export function isId(key: string): boolean {
	return /^[0-9a-f]{16}$/.test(key);
}

/** What one item of a list field is called, its option: `identities` gives `identity`. */
export function singular(name: string): string {
	if (name.endsWith('ies')) return `${name.slice(0, -3)}y`;
	return name.endsWith('s') ? name.slice(0, -1) : name;
}

const isSecret = (field: Field) => field.secret === true;
const isEditable = (field: Field) => field.editable;
const itemFields = (field: Field): Field[] => field.fields ?? [];

/** The item field naming an item: its first required `choice`. */
function keyOf(field: Field): Field {
	const key = itemFields(field).find((item) => item.required && item.type === 'choice');
	return key ?? fail(`the catalog names no required choice among ${field.name}'s item fields`);
}

/** Each list of objects by the option that adds one of its items, `--identity`. */
function itemOptions(kind: Description): Map<string, Field> {
	const options = new Map<string, Field>();
	for (const field of kind.fields) {
		if (itemFields(field).length === 0) continue;
		const option = singular(field.name);
		if (WRITE_OPTIONS.includes(option)) fail(`${field.name}'s items cannot be named --${option}`);
		options.set(option, field);
	}
	return options;
}

function listed(kinds: readonly Description[]): string {
	return kinds.map((kind) => `${kind.name} (${kind.plural})`).join(', ');
}

/** The kind `word` names, by its name or its plural. */
function kindNamed(kinds: readonly Description[], word: string): Description {
	const kind = kinds.find((one) => word === one.name || word === one.plural);
	return kind ?? fail(`${word} is no kind the registry holds; it holds ${listed(kinds)}`);
}

const marks = (field: Field) =>
	(['required', 'secret'] as const).filter((word) => field[word] === true);

/** The kind's fields, one a line, as `--help` shows them. */
export function described(kind: Description): string {
	const lines = [`${kind.name}'s fields, from the registry's catalog:`];
	for (const field of kind.fields) {
		const facts: string[] = [field.type];
		if (field.kind) facts.push(`of a ${field.kind}, by id or name`);
		if (field.choices?.length) facts.push(`one of ${field.choices.join('|')}`);
		facts.push(...marks(field));
		if (!isEditable(field)) facts.push('not editable');
		lines.push(`  ${field.name}: ${facts.join(', ')}`);
		for (const item of itemFields(field)) {
			const choices = item.choices?.length ? `, one of ${item.choices.join('|')}` : '';
			const also = [...marks(item), ...(isEditable(item) ? [] : ['not editable'])];
			lines.push(`    ${item.name}: ${item.type}${choices}${also.map((m) => `, ${m}`).join('')}`);
		}
	}
	return lines.join('\n');
}

function usage(kind: Description, verb: Verb): string {
	const key = verb === 'list' ? '' : verb === 'create' ? ' <name>' : ' <name-or-id>';
	if (verb === 'list' || verb === 'get') {
		return `usage: mise run ${APP} ${kind.name} ${verb}${key} [--json]\n\n${described(kind)}`;
	}
	const items = [...itemOptions(kind)].map(
		([option, field]) => ` [--${option} ${keyOf(field).name.toUpperCase()}[,FIELD=VALUE...]]`,
	);
	const unset = verb === 'patch' ? ' [--unset FIELD]' : '';
	return [
		`usage: mise run ${APP} ${kind.name} ${verb}${key} [--set FIELD=VALUE] [--json FIELD=JSON]` +
			`${unset}${items.join('')} [--dry-run]`,
		'',
		'  --set FIELD=VALUE  a scalar, typed by the catalog; a list of choices comma-separated; a',
		'                     secret field named alone, `--set field`, and typed in',
		'  --json FIELD=JSON  any field, as JSON',
		...(unset ? ['  --unset FIELD      take a field out'] : []),
		...(items.length > 0
			? ['  --<item> KEY       one item, its secret fields typed in; on patch it replaces the one']
			: []),
		...(items.length > 0 ? ['                     of that key and keeps the rest'] : []),
		'  --dry-run, -n      say what, send nothing',
		'',
		described(kind),
	].join('\n');
}

/** What a verb was given, by option. */
export interface Given {
	key: string;
	/** A read's `--json`: the answer as it is. */
	raw: boolean;
	set: string[];
	json: string[];
	unset: string[];
	items: Map<string, string[]>;
	dry: boolean;
}

/** The options of `verb` on `kind`, refusing any the catalog does not make; none for `--help`. */
function given(kind: Description, verb: Verb, args: string[]): Given | undefined {
	const write = verb === 'create' || verb === 'patch';
	const items = write ? itemOptions(kind) : new Map<string, Field>();
	const options: NonNullable<Parameters<typeof parseArgs>[0]>['options'] = {
		help: { type: 'boolean', short: 'h' },
		json: write ? { type: 'string', multiple: true } : { type: 'boolean' },
	};
	if (write) {
		options.set = { type: 'string', multiple: true };
		options['dry-run'] = { type: 'boolean', short: 'n' };
		if (verb === 'patch') options.unset = { type: 'string', multiple: true };
		for (const option of items.keys()) options[option] = { type: 'string', multiple: true };
	}
	let parsed;
	try {
		parsed = parseArgs({ args, options, allowPositionals: true, strict: true });
	} catch (error) {
		fail(`${(error as Error).message}\n${usage(kind, verb)}`);
	}
	const { values, positionals } = parsed;
	if (values.help) return undefined;
	if (positionals.length !== (verb === 'list' ? 0 : 1)) fail(usage(kind, verb));
	const strings = (name: string) => (write ? (values[name] as string[] | undefined) : []) ?? [];
	return {
		key: positionals[0] ?? '',
		raw: !write && values.json === true,
		set: strings('set'),
		json: strings('json'),
		unset: strings('unset'),
		items: new Map([...items].map(([option, field]) => [field.name, strings(option)])),
		dry: values['dry-run'] === true,
	};
}

type Resolve = (kind: string, name: string) => Promise<string>;

/** `text` as `field` holds it, refused before anything is sent when it cannot be. */
async function typed(field: Field, text: string, resolve: Resolve): Promise<unknown> {
	const { name, type } = field;
	const choices = field.choices ?? [];
	switch (type) {
		case 'integer':
			if (!/^[+-]?\d+$/.test(text.trim())) fail(`${name} is an integer, not '${text}'`);
			return Number.parseInt(text, 10);
		case 'boolean':
			if (text !== 'true' && text !== 'false') fail(`${name} is true or false, not '${text}'`);
			return text === 'true';
		case 'choice':
			if (!choices.includes(text)) fail(`${name} is one of ${choices.join(', ')}, not '${text}'`);
			return text;
		case 'reference':
			return isId(text) ? text : resolve(field.kind ?? fail(`${name} names no kind`), text);
		case 'string':
			return text;
		case 'list': {
			if (itemFields(field).length > 0) break;
			const values = text
				.split(',')
				.map((value) => value.trim())
				.filter(Boolean);
			if (choices.length > 0) {
				const wrong = values.filter((value) => !choices.includes(value));
				if (wrong.length > 0) {
					fail(`${name} holds each of ${choices.join(', ')}, never ${wrong.join(', ')}`);
				}
				if (new Set(values).size !== values.length) fail(`${name} holds each choice once`);
			}
			return values;
		}
		case 'object':
			break;
	}
	const what = itemFields(field).length > 0 ? 'a list of objects' : `an ${type}`;
	fail(`${name} is ${what}; give it by --json`);
}

function split(pair: string, option: string): [string, boolean, string] {
	const at = pair.indexOf('=');
	const field = at < 0 ? pair : pair.slice(0, at);
	if (!field) fail(`${option} takes FIELD=VALUE, not '${pair}'`);
	return [field, at >= 0, at < 0 ? '' : pair.slice(at + 1)];
}

const NEVER_FROM_ARGUMENTS = 'typed in on the terminal and never taken from an argument';

/** A secret typed in on the terminal, never echoed; a dry run asks for none. */
async function asked(
	world: World,
	prompt: string,
	required: boolean,
	dry: boolean,
): Promise<string | undefined> {
	if (dry) return HIDDEN;
	const value = await world.typedIn(`${prompt}: `);
	if (!value && required) fail(`${prompt} is required`);
	return value || undefined;
}

/**
 * The list `field` is written as: each item given, named by its key and with its secret fields
 * typed in; on a patch the current items of every other key kept, in order.
 */
async function items(
	world: World,
	field: Field,
	texts: string[],
	current: unknown,
	resolve: Resolve,
	dry: boolean,
): Promise<Record<string, unknown>[]> {
	const key = keyOf(field);
	const fields = new Map(itemFields(field).map((item) => [item.name, item]));
	const option = `--${singular(field.name)}`;
	const made = new Map<unknown, Record<string, unknown>>();
	for (const text of texts) {
		const [value = '', ...pairs] = text.split(',');
		const named = await typed(key, value, resolve);
		if (made.has(named)) fail(`${field.name} names ${value} twice`);
		const item: Record<string, unknown> = { [key.name]: named };
		for (const pair of pairs) {
			const [name, equals, raw] = split(pair, option);
			const inner = fields.get(name);
			if (inner === undefined) {
				fail(`${name} is not a field of ${field.name}'s items; they hold ${[...fields.keys()].join(', ')}`);
			}
			if (isSecret(inner)) fail(`${name} is secret, ${NEVER_FROM_ARGUMENTS}`);
			if (!isEditable(inner)) fail(`${name} is not editable; the registry writes it`);
			if (!equals) fail(`${option} takes ${name}=VALUE`);
			item[name] = await typed(inner, raw, resolve);
		}
		for (const [name, inner] of fields) {
			if (!isSecret(inner)) continue;
			const secret = await asked(world, `${singular(field.name)} ${value}: ${name}`, inner.required, dry);
			if (secret !== undefined) item[name] = secret;
		}
		made.set(named, item);
	}
	const old = Array.isArray(current) ? (current as Record<string, unknown>[]) : [];
	const kept = old.filter((item) => !made.has(item[key.name]));
	return [...kept, ...made.values()];
}

/** Whether a JSON value given for `field` carries a secret, at the top or in an item. */
function secretInside(field: Field | undefined, value: unknown): boolean {
	if (field === undefined) return false;
	if (isSecret(field)) return true;
	const secrets = itemFields(field)
		.filter(isSecret)
		.map((item) => item.name);
	if (secrets.length === 0 || !Array.isArray(value)) return false;
	return value.some(
		(item) =>
			typeof item === 'object' &&
			item !== null &&
			secrets.some((name) => Object.hasOwn(item, name)),
	);
}

/**
 * The spec a write sends, from the options given: on a create (`current` undefined) the whole of
 * it, on a patch the merge patch of what changes, `null` taking a key out.
 */
export async function changes(
	world: World,
	kind: Description,
	given: Given,
	current: Record<string, unknown> | undefined,
	resolve: Resolve,
): Promise<Record<string, unknown>> {
	const fields = new Map(kind.fields.map((field) => [field.name, field]));
	const spec: Record<string, unknown> = {};
	const prompt = (name: string) => `${kind.name} ${given.key}: ${name}`;
	const put = (name: string, value: unknown) => {
		if (Object.hasOwn(spec, name)) fail(`${name} is given twice`);
		const known = fields.get(name);
		if (known !== undefined && !isEditable(known)) fail(`${name} is not editable`);
		spec[name] = value;
	};

	for (const pair of given.set) {
		const [name, equals, value] = split(pair, '--set');
		const field = fields.get(name);
		if (field === undefined) {
			fail(`${name} is not a field of a ${kind.name}; --json ${name}=<json> sends it as given`);
		}
		if (isSecret(field)) {
			if (equals) fail(`${name} is secret, ${NEVER_FROM_ARGUMENTS}`);
			put(name, await asked(world, prompt(name), true, given.dry));
			continue;
		}
		if (!equals) fail(`--set takes ${name}=VALUE`);
		put(name, await typed(field, value, resolve));
	}
	for (const pair of given.json) {
		const [name, , text] = split(pair, '--json');
		let value: unknown;
		try {
			value = JSON.parse(text);
		} catch {
			fail(`--json ${name}= takes JSON, not '${text}'`);
		}
		if (secretInside(fields.get(name), value)) fail(`${name} carries a secret, ${NEVER_FROM_ARGUMENTS}`);
		put(name, value);
	}
	for (const name of given.unset) {
		if (fields.get(name)?.required) fail(`${name} is required, so it is changed and never taken out`);
		put(name, null);
	}
	for (const field of fields.values()) {
		const texts = given.items.get(field.name) ?? [];
		if (texts.length === 0) continue;
		put(field.name, await items(world, field, texts, current?.[field.name], resolve, given.dry));
	}
	if (current === undefined) {
		for (const field of fields.values()) {
			if (Object.hasOwn(spec, field.name) || !field.required) continue;
			if (isSecret(field)) {
				spec[field.name] = await asked(world, prompt(field.name), true, given.dry);
			} else if (itemFields(field).length > 0) {
				fail(`${field.name} is required: --${singular(field.name)} <${keyOf(field).name}>`);
			} else {
				fail(`${field.name} is required: --set ${field.name}=<${field.type}>`);
			}
		}
	}
	return spec;
}

/** The registry's envelope at `address`; undefined when it did not answer at all. */
async function call(
	world: World,
	address: string,
	method: string,
	path: string,
	body?: unknown,
	secret?: string,
): Promise<Envelope | undefined> {
	const headers: Record<string, string> = { 'Content-Type': 'application/json' };
	if (secret) headers.Authorization = `Bearer ${secret}`;
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

/** The envelope of the first core to answer, in order, each found only when asked. */
async function ask(
	world: World,
	method: string,
	path: string,
	body?: unknown,
	secret?: string,
): Promise<Envelope> {
	for (const node of world.placements()) {
		const answer = await call(world, world.address(node), method, path, body, secret);
		if (answer !== undefined) return answer;
	}
	fail(`no registry answered ${method} ${path}`);
}

/** The `data` of a success; a refusal fails, with its code and message. */
function dataOf(answer: Envelope, what: string): unknown {
	if (answer.status === 'success') return answer.data;
	fail(`${what}: refused ${answer.code}: ${answer.message}`);
}

/** The catalog, held to the reader generated from catalog.schema.json before anything reads it. */
async function catalog(world: World): Promise<Description[]> {
	const data = dataOf(await ask(world, 'GET', '/v1/kinds'), 'GET /v1/kinds');
	const read = v.safeParse(Catalog, data);
	if (!read.success) {
		fail(`the registry's catalog is not what catalog.schema.json describes:\n${v.summarize(read.issues)}`);
	}
	return read.output.items;
}

/** A value as a table shows it: an id by its first eight digits. */
export function cell(value: unknown, field?: Field): string {
	if (value === null || value === undefined) return '-';
	if (typeof value === 'boolean' || typeof value === 'number') return String(value);
	if (typeof value === 'string') {
		return field?.type === 'reference' && isId(value) ? value.slice(0, SHORT) : value;
	}
	if (Array.isArray(value) && value.every((one) => typeof one !== 'object' || one === null)) {
		return value.map((one) => cell(one)).join(',');
	}
	return JSON.stringify(value);
}

export function table(kind: Description, resources: readonly Resource[]): string {
	const fields = new Map(kind.fields.map((field) => [field.name, field]));
	const rows = [['id', 'name', ...kind.columns]];
	for (const resource of resources) {
		const spec = resource.spec ?? {};
		rows.push([
			resource.id.slice(0, SHORT),
			resource.name,
			...kind.columns.map((column) => cell(spec[column], fields.get(column))),
		]);
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

/** One resource, its spec a field a line, the catalog's order first and every other key after. */
export function readable(kind: Description, resource: Resource): string {
	const spec = resource.spec ?? {};
	const known = kind.fields.map((field) => field.name);
	const names = [
		...known.filter((name) => Object.hasOwn(spec, name)),
		...Object.keys(spec)
			.filter((name) => !known.includes(name))
			.sort(),
	];
	const fields = new Map(kind.fields.map((field) => [field.name, field]));
	const width = Math.max(...['id', 'revision', ...names].map((name) => name.length));
	const lines = [
		`${kind.name} ${resource.name}`,
		`  ${'id'.padEnd(width)}  ${resource.id}`,
		`  ${'revision'.padEnd(width)}  ${resource.revision}`,
	];
	for (const name of names) {
		const value = spec[name];
		if (Array.isArray(value) && value.length > 0 && value.every((one) => isObject(one))) {
			lines.push(`  ${name}`);
			for (const item of value as Record<string, unknown>[]) {
				const pairs = Object.entries(item).map(([key, one]) => `${key}=${cell(one)}`);
				lines.push(`    ${pairs.join('  ')}`);
			}
		} else if (typeof value === 'string') {
			lines.push(`  ${name.padEnd(width)}  ${value}`);
		} else {
			lines.push(`  ${name.padEnd(width)}  ${cell(value, fields.get(name))}`);
		}
	}
	return lines.join('\n');
}

const isObject = (value: unknown): value is Record<string, unknown> =>
	typeof value === 'object' && value !== null && !Array.isArray(value);

async function write(
	world: World,
	method: string,
	path: string,
	body: unknown,
	what: string,
	dry: boolean,
): Promise<void> {
	if (dry) {
		world.print(
			`${APP}: would ${method} ${path}, to the first of ${world.placements().join(', ')} whose` +
				` registry answers on ${world.port}, as Bearer <redacted>:`,
		);
		world.print(JSON.stringify(body, null, 2));
		return;
	}
	const secret = world.token();
	if (!secret) fail(`secrets.json has no ${TOKEN}; \`registry token\` makes it`);
	const data = dataOf(await ask(world, method, path, body, secret), what);
	if (isObject(data) && 'task' in data) {
		world.print(`${APP}: ${what}: written, revision ${data.revision}, task ${data.task}`);
	} else {
		world.print(`${APP}: ${what}: unchanged`);
	}
}

function usageOf(kinds: readonly Description[]): string {
	return (
		`name a kind and a verb: \`mise run ${APP} <kind> ${VERBS.join('|')} [name-or-id]\`, a kind` +
		` being one of ${listed(kinds)}; \`<kind> <verb> --help\` says its fields`
	);
}

const isVerb = (word: string | undefined): word is Verb => VERBS.includes(word as Verb);

/** The task: what it prints goes through `world`, and a refusal is thrown as a `Failure`. */
export async function run(world: World, args: readonly string[]): Promise<void> {
	const kinds = await catalog(world);
	const [word, verb, ...rest] = args;
	if (word === undefined || !isVerb(verb)) {
		if (word !== undefined && word !== '-h' && word !== '--help') {
			world.print(described(kindNamed(kinds, word)));
		}
		fail(usageOf(kinds));
	}
	const kind = kindNamed(kinds, word);
	const options = given(kind, verb, rest);
	if (options === undefined) {
		world.print(usage(kind, verb));
		return;
	}
	const plural = kind.plural;
	if (verb === 'list') {
		const found = dataOf(await ask(world, 'GET', `/v1/${plural}`), `GET /v1/${plural}`);
		const resources = (found as { items: Resource[] }).items;
		world.print(options.raw ? JSON.stringify(resources, null, 2) : table(kind, resources));
		return;
	}
	const what = `${kind.name} ${options.key}`;
	if (verb === 'get') {
		const path = `/v1/${plural}/${options.key}`;
		const resource = dataOf(await ask(world, 'GET', path), what) as Resource;
		world.print(options.raw ? JSON.stringify(resource, null, 2) : readable(kind, resource));
		return;
	}
	const resolve: Resolve = async (referenced, name) => {
		const path = `/v1/${kindNamed(kinds, referenced).plural}/${name}`;
		return (dataOf(await ask(world, 'GET', path), `${referenced} ${name}`) as Resource).id;
	};
	if (verb === 'create') {
		if (isId(options.key)) {
			fail(`${options.key} has an id's shape, which a route reads as an id, never a name`);
		}
		const spec = await changes(world, kind, options, undefined, resolve);
		const body = { by: BY, revision: 0, spec };
		await write(world, 'PUT', `/v1/${plural}/${options.key}`, body, what, options.dry);
		return;
	}
	const resource = dataOf(await ask(world, 'GET', `/v1/${plural}/${options.key}`), what) as Resource;
	const spec = await changes(world, kind, options, resource.spec ?? {}, resolve);
	if (Object.keys(spec).length === 0) {
		fail('nothing to change: give --set, --json, --unset or an item');
	}
	const body = { by: BY, revision: resource.revision, spec };
	const named = `${kind.name} ${resource.name}`;
	await write(world, 'PATCH', `/v1/${plural}/${resource.id}`, body, named, options.dry);
}

/** Lines of standard input when it is not a terminal, read one per secret asked for. */
let piped: AsyncIterator<string> | undefined;

/** `prompt` on standard error, then what is typed, never echoed. */
async function typedIn(prompt: string): Promise<string> {
	process.stderr.write(prompt);
	const input = process.stdin;
	if (!input.isTTY) {
		piped ??= createInterface({ input, terminal: false })[Symbol.asyncIterator]();
		const line = await piped.next();
		process.stderr.write('\n');
		return line.done ? '' : line.value;
	}
	input.setRawMode(true);
	input.resume();
	return new Promise((resolve) => {
		let value = '';
		const done = () => {
			input.off('data', typing);
			input.setRawMode(false);
			input.pause();
			process.stderr.write('\n');
		};
		const typing = (chunk: Buffer) => {
			for (const key of chunk.toString('utf8')) {
				if (key === '\r' || key === '\n' || key === '\u0004') {
					done();
					resolve(value);
					return;
				}
				if (key === '\u0003') {
					done();
					process.exit(130);
				}
				value = key === '\u007f' || key === '\b' ? value.slice(0, -1) : value + key;
			}
		};
		input.on('data', typing);
	});
}

/** The registry's cores, from its service.toml. */
function placements(): string[] {
	const declared = parse(readFileSync(join(import.meta.dirname, '../service.toml'), 'utf8'));
	const cores = declared.placements;
	if (!Array.isArray(cores) || !cores.every((core) => typeof core === 'string')) {
		fail("the registry's service.toml places it on no nodes");
	}
	return cores;
}

/** A node's tailnet IPv4, as tailscale knows it. */
function address(node: string): string {
	let found = '';
	try {
		found = execFileSync('tailscale', ['ip', '-4', node], { encoding: 'utf8' }).trim();
	} catch {
		// Said below, with the node's name.
	}
	return found.split(/\s+/)[0] || fail(`tailscale has no IPv4 for ${node}`);
}

const WORLD: World = {
	placements,
	address,
	port: PORT,
	token: () => process.env[TOKEN],
	typedIn,
	print: (text) => console.log(text),
};

if (import.meta.main) {
	try {
		await run(WORLD, process.argv.slice(2));
	} catch (error) {
		if (!(error instanceof Failure)) throw error;
		console.error(`${APP}: ${error.message}`);
		process.exitCode = 1;
	} finally {
		// Standard input read for a secret holds the process open until it is let go.
		await piped?.return?.();
	}
}
