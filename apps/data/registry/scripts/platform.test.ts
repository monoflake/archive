import { readdirSync, readFileSync } from 'node:fs';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { join } from 'node:path';
import * as v from 'valibot';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { Catalog, type Description, type Field } from '../../../control/control/src/catalog.gen.ts';
import { CATALOG as HELD } from '../../../control/control/src/kinds.ts';
import { Failure, cell, nodeIds, placedOn, run, singular, type World } from './platform.ts';

// The fake answers the catalog control holds, never a copy of it -- RFC bq3vn.
const CATALOG = structuredClone(HELD.items);
const EXAMPLES = join(import.meta.dirname, '../examples/catalog');

/**
 * The catalog with an identity's value marked `secret` and the name it would be kept under beside
 * it, as a written-only field would be: no field is one today, and the path it takes is kept for
 * the first that is.
 */
const WITH_A_SECRET = structuredClone(CATALOG).map((kind) =>
	kind.name !== 'account'
		? kind
		: {
				...kind,
				fields: kind.fields.map((field) =>
					field.name !== 'identities'
						? field
						: {
								...field,
								fields: [
									field.fields![0]!,
									{
										name: 'value',
										type: 'string' as const,
										secret: true,
										editable: true,
										required: false,
									},
									{ name: 'secret', type: 'string' as const, editable: false, required: false },
								],
							},
				),
			},
);

const TOKEN = 'token-for-the-fake';
const READ = 'read-token-for-the-fake';
const SECRET_VALUE = 'me-at-example-dot-com';
const ACCOUNT_ID = 'a3f8c91d7e42b650';
/** An account's identities as the registry keeps and answers them. */
const IDENTITIES = [
	{ kind: 'username', value: 'ocid1.user.oc1..example' },
	{ kind: 'email', value: SECRET_VALUE },
];
const NODE_ID = 'abd3922110a78032';
const SIN_ID = '9d399312d1c3ddc4';
/** Each node's id by its name, as nodes.toml holds them. */
const IDS = new Map([
	['tyo', NODE_ID],
	['sin', SIN_ID],
]);

interface Resource {
	kind: string;
	id: string;
	name: string | null;
	revision: number;
	spec: Record<string, unknown>;
}

interface Sent {
	method: string;
	path: string;
	auth: string | undefined;
	body: { by: string; revision: number; spec: Record<string, unknown> } | undefined;
}

/** What the fake answers from, and every request it was sent. */
class Control {
	revision = 10;
	requests: Sent[] = [];
	refusal: [string, string] | undefined;
	catalog: Description[] = CATALOG;
	held: Record<string, Record<string, Resource>>;

	constructor() {
		this.held = {
			nodes: {
				[NODE_ID]: this.resource('node', NODE_ID, null, {
					city: 'Tokyo',
					country: 'JP',
					continent: 'AS',
					address: '100.64.0.1',
					tier: 'datacenter',
					account: ACCOUNT_ID,
					system: 'debian',
					retired: false,
					roles: ['compute', 'core'],
					grants: ['hub:peer'],
				}),
				[SIN_ID]: this.resource('node', SIN_ID, null, {
					city: 'Singapore',
					country: 'SG',
					continent: 'AS',
					address: '100.64.0.2',
					tier: 'datacenter',
					system: 'debian',
				}),
			},
			apps: {},
			accounts: {
				[ACCOUNT_ID]: this.resource('account', ACCOUNT_ID, null, {
					provider: 'oracle',
					identities: structuredClone(IDENTITIES),
				}),
			},
		};
	}

	resource(kind: string, id: string, name: string | null, spec: Record<string, unknown>): Resource {
		return { kind, id, name, revision: this.revision, spec };
	}

	find(plural: string, key: string): Resource | undefined {
		return Object.values(this.held[plural] ?? {}).find((one) => key === one.id || key === one.name);
	}

	answer(sent: Sent): [number, unknown] {
		this.requests.push(sent);
		if (sent.method === 'GET' && sent.auth !== `Bearer ${READ}`) {
			return [401, { status: 'error', code: 'invalid_token', message: 'No read token' }];
		}
		if (!sent.path.startsWith('/v1/registry/')) {
			return [404, { status: 'error', code: 'no_such_route', message: 'No such route' }];
		}
		const parts = sent.path.replace(/^\/v1\/registry\/|\/$/g, '').split('/');
		parts.unshift('v1');
		if (parts.join('/') === 'v1/kinds') {
			return [200, { status: 'success', data: { items: this.catalog } }];
		}
		const plural = parts[1] ?? '';
		const held = this.held[plural];
		if (held === undefined) {
			return [404, { status: 'error', code: 'no_such_kind', message: 'No such kind' }];
		}
		if (parts.length === 2 && sent.method === 'GET') {
			const items = Object.values(held);
			return [200, { status: 'success', data: { items, revision: this.revision } }];
		}
		const key = parts[2];
		const found = key === undefined ? undefined : this.find(plural, key);
		if (sent.method === 'GET') {
			if (found === undefined) {
				return [404, { status: 'error', code: 'no_such_resource', message: 'None held' }];
			}
			return [200, { status: 'success', data: found }];
		}
		if (sent.auth !== `Bearer ${TOKEN}`) {
			return [401, { status: 'error', code: 'invalid_token', message: 'No write token' }];
		}
		if (this.refusal) {
			return [400, { status: 'error', code: this.refusal[0], message: this.refusal[1] }];
		}
		const body = sent.body!;
		if (body.revision !== (found?.revision ?? 0)) {
			return [409, { status: 'error', code: 'invalid_revision', message: 'Changed since' }];
		}
		let spec = body.spec;
		if (sent.method === 'PATCH') {
			spec = structuredClone(found!.spec);
			for (const [name, value] of Object.entries(body.spec)) {
				if (value === null) delete spec[name];
				else spec[name] = value;
			}
		}
		if (found !== undefined && JSON.stringify(spec) === JSON.stringify(found.spec)) {
			return [200, { status: 'success', data: {} }];
		}
		this.revision += 1;
		const kind = this.catalog.find((one) => one.plural === plural)!.name;
		const id = `${this.revision.toString(16).padStart(8, '0')}${'0'.repeat(8)}`;
		const made = found ?? this.resource(kind, id, key ?? null, {});
		made.spec = spec;
		made.revision = this.revision;
		held[made.id] = made;
		const data = { revision: this.revision, task: '00000000000000f1' };
		return [202, { status: 'success', data: sent.method === 'POST' ? { ...data, id } : data }];
	}
}

let state: Control;
let server: Server;
let printed: string[];
let asked: string[];
let token: string | undefined;
let read: string | undefined;

beforeEach(async () => {
	state = new Control();
	printed = [];
	asked = [];
	token = TOKEN;
	read = READ;
	server = createServer((request, response) => {
		let text = '';
		request.on('data', (chunk: Buffer) => (text += chunk.toString()));
		request.on('end', () => {
			const [code, answer] = state.answer({
				method: request.method ?? '',
				path: request.url ?? '',
				auth: request.headers.authorization,
				body: text ? JSON.parse(text) : undefined,
			});
			response.writeHead(code, { 'Content-Type': 'application/json' });
			response.end(JSON.stringify(answer));
		});
	});
	await new Promise<void>((done) => server.listen(0, '127.0.0.1', done));
});

afterEach(async () => {
	await new Promise((done) => server.close(done));
});

/** Where the fake listens, as `host:port`. */
function listening(): string {
	const { address, port } = server.address() as AddressInfo;
	return `${address}:${port}`;
}

function world(): World {
	return {
		control: `http://${listening()}`,
		readToken: () => read,
		writeToken: () => token,
		nodes: () => IDS,
		typedIn: async (prompt) => {
			asked.push(prompt);
			return SECRET_VALUE;
		},
		print: (text) => printed.push(text),
	};
}

/** What the task printed, and the message it failed with, which the shell's exit is 1 for. */
async function task(...args: string[]): Promise<{ out: string; failed: string | undefined }> {
	let failed: string | undefined;
	try {
		await run(world(), args);
	} catch (error) {
		if (!(error instanceof Failure)) throw error;
		failed = error.message;
	}
	return { out: printed.join('\n'), failed };
}

const writes = () => state.requests.filter((sent) => sent.method !== 'GET');

/** The message of a refusal made before anything was sent. */
async function refused(...args: string[]): Promise<string> {
	const { failed } = await task(...args);
	expect(failed, 'a failure exits 1 with a message').toBeTypeOf('string');
	expect(writes(), 'a refusal before sending sends nothing').toEqual([]);
	return failed!;
}

/** The JSON a dry run printed after its line. */
const shown = (out: string) => JSON.parse(out.slice(out.indexOf('{')));

describe('reading', () => {
	it('lists the catalog columns, ids by their first eight digits, with the read token', async () => {
		const { out, failed } = await task('nodes', 'list');
		expect(failed).toBeUndefined();
		const [head, row] = out.split('\n');
		const columns = ['id', 'city', 'country', 'address', 'tier', 'account', 'system'];
		expect(head!.split(/\s+/)).toEqual(columns);
		expect(row!.split(/\s+/)).toEqual([
			NODE_ID.slice(0, 8),
			'Tokyo',
			'JP',
			'100.64.0.1',
			'datacenter',
			ACCOUNT_ID.slice(0, 8),
			'debian',
		]);
		expect(state.requests.every((sent) => sent.auth === `Bearer ${READ}`)).toBe(true);
	});

	it('reads nothing without the read token', async () => {
		read = '';
		expect(await refused('nodes', 'list')).toContain('CONTROL_READ_TOKEN');
		expect(state.requests).toEqual([]);
	});

	it('lists the items as answered with --json', async () => {
		const { out } = await task('node', 'list', '--json');
		expect(JSON.parse(out)).toEqual(Object.values(state.held.nodes!));
	});

	it("gets a node by its name, its spec in the catalog's order, then the rest", async () => {
		const { out } = await task('node', 'get', 'tyo');
		const lines = out.split('\n');
		expect(lines[0]).toBe(`node ${NODE_ID.slice(0, 8)}`);
		expect(lines[1], 'get shows the id whole').toContain(NODE_ID);
		const names = lines.slice(3).map((line) => line.trim().split(/\s+/)[0]);
		const order = ['city', 'country', 'continent', 'address', 'tier', 'system', 'retired'];
		expect(names).toEqual([...order, 'account', 'roles', 'grants']);
		expect(out).toContain('compute,core');
		expect(state.requests.at(-1)!.path).toBe(`/v1/registry/nodes/${NODE_ID}`);
	});

	it('gets by id, as JSON', async () => {
		const { out } = await task('node', 'get', NODE_ID, '--json');
		expect(JSON.parse(out).id).toBe(NODE_ID);
	});

	it('refuses a node by a name nodes.toml does not hold, asking control nothing of it', async () => {
		expect(await refused('node', 'get', 'zzz')).toContain('nodes.toml names no such node');
		expect(state.requests.map((sent) => sent.path)).toEqual(['/v1/registry/kinds']);
	});

	it('fails on a kind the catalog does not hold, naming those it does', async () => {
		expect(await refused('widgets', 'list')).toContain('node (nodes)');
	});

	it('fails on a resource not held, with its code', async () => {
		expect(await refused('app', 'get', 'zzz')).toContain('no_such_resource');
	});

	it('refuses an option the catalog does not make', async () => {
		expect(await refused('app', 'create', 'web', '--identity', 'email')).toContain('--identity');
	});

	it('lists a kind with no name by its id and its columns', async () => {
		const { out } = await task('accounts', 'list');
		const [head, row] = out.split('\n');
		expect(head!.split(/\s+/)).toEqual(['id', 'provider']);
		expect(row!.split(/\s+/)).toEqual([ACCOUNT_ID.slice(0, 8), 'oracle']);
	});

	it('gets by the first eight digits of an id or more, for every kind', async () => {
		for (const [kind, id] of [
			['account', ACCOUNT_ID],
			['node', NODE_ID],
		] as const) {
			for (const key of [id.slice(0, 8), id.slice(0, 12), id]) {
				printed = [];
				const { out, failed } = await task(kind, 'get', key, '--json');
				expect(failed, `${kind} ${key}`).toBeUndefined();
				expect(JSON.parse(out).id).toBe(id);
			}
		}
		printed = [];
		const { out } = await task('account', 'get', ACCOUNT_ID.slice(0, 8));
		expect(out.split('\n')[0]).toBe(`account ${ACCOUNT_ID.slice(0, 8)}`);
	});

	it('refuses a start of an id two share, naming both', async () => {
		const other = `${ACCOUNT_ID.slice(0, 10)}000000`;
		state.held.accounts![other] = state.resource('account', other, null, { provider: 'azure' });
		const failed = await refused('account', 'get', ACCOUNT_ID.slice(0, 8));
		expect(failed).toContain(ACCOUNT_ID);
		expect(failed).toContain(other);
		printed = [];
		const { out } = await task('account', 'get', ACCOUNT_ID.slice(0, 11), '--json');
		expect(JSON.parse(out).id).toBe(ACCOUNT_ID);
	});

	it('refuses a name for a kind with none, and a start no id has', async () => {
		expect(await refused('account', 'get', 'oci')).toContain('no name');
		expect(await refused('account', 'get', 'ffffffff')).toContain('no account has an id');
		expect(await refused('account', 'get', 'a3f8')).toContain('no name');
	});
});

describe('writing', () => {
	it('creates by PUT at revision 0, as operator, placed on node ids given by name', async () => {
		const { out, failed } = await task('app', 'create', 'ledger', '--set', 'placements=tyo,sin');
		expect(failed).toBeUndefined();
		const [sent] = writes();
		expect([sent!.method, sent!.path]).toEqual(['PUT', '/v1/registry/apps/ledger']);
		expect(sent!.auth).toBe(`Bearer ${TOKEN}`);
		const spec = { placements: [NODE_ID, SIN_ID] };
		expect(sent!.body).toEqual({ by: 'operator', revision: 0, spec });
		expect(out).toContain('written, revision 11, task 00000000000000f1');
	});

	it('places on a node given by its id or its start, each once', async () => {
		const given = `placements=${SIN_ID.slice(0, 8)},${NODE_ID}`;
		const { failed } = await task('app', 'create', 'ledger', '--set', given);
		expect(failed).toBeUndefined();
		expect(writes()[0]!.body!.spec.placements).toEqual([SIN_ID, NODE_ID]);
		expect(await task('app', 'create', 'web', '--set', 'placements=tyo,zzz')).toMatchObject({
			failed: expect.stringContaining('node zzz: nodes.toml names no such node'),
		});
		expect(await task('app', 'create', 'web', '--set', `placements=tyo,${NODE_ID}`)).toMatchObject({
			failed: expect.stringContaining('names each node once'),
		});
		expect(writes()).toHaveLength(1);
	});

	it("resolves a reference by its id's first eight digits and types every scalar", async () => {
		const account = `account=${ACCOUNT_ID.slice(0, 8)}`;
		const place = ['city=Osaka', 'country=JP', 'continent=AS'];
		const scalars = [...place, 'address=100.64.0.3', 'tier=datacenter', account, 'system=alpine'];
		const set = [...scalars, 'expiry=2036', 'core=true'].flatMap((pair) => ['--set', pair]);
		const { failed } = await task('node', 'create', ...set, '--json', 'grants=["hub:peer"]');
		expect(failed).toBeUndefined();
		const [sent] = writes();
		expect([sent!.method, sent!.path]).toEqual(['POST', '/v1/registry/nodes']);
		const spec = sent!.body!.spec;
		expect(spec.account).toBe(ACCOUNT_ID);
		expect([spec.expiry, spec.core, spec.grants]).toEqual([2036, true, ['hub:peer']]);
	});

	it('fails a create without a required field', async () => {
		expect(await refused('app', 'create', 'ledger')).toContain('placements is required');
	});

	it("fails a create of a name of an id's shape", async () => {
		await refused('app', 'create', '0123456789abcdef');
		await refused('app', 'create', 'deadbeef');
	});

	it.each([
		['expiry=soon', 'integer'],
		['retired=yes', 'true or false'],
		['tier=moon', 'one of'],
		['roles=compute,gpu', 'never gpu'],
		['roles=core,core', 'once'],
		['release=x', '--json'],
		['color=red', 'not a field'],
	])('types --set %s before anything is sent', async (pair, said) => {
		expect(await refused('node', 'patch', 'tyo', '--set', pair)).toContain(said);
	});

	it('patches at the revision read, a merge patch', async () => {
		const given = ['--set', 'retired=true', '--unset', 'roles'];
		const { out, failed } = await task('node', 'patch', 'tyo', ...given);
		expect(failed, out).toBeUndefined();
		const [sent] = writes();
		expect([sent!.method, sent!.path]).toEqual(['PATCH', `/v1/registry/nodes/${NODE_ID}`]);
		const spec = { retired: true, roles: null };
		expect(sent!.body).toEqual({ by: 'operator', revision: 10, spec });
		expect(state.held.nodes![NODE_ID]!.spec).not.toHaveProperty('roles');
	});

	it('never unsets a required field', async () => {
		expect(await refused('node', 'patch', 'tyo', '--unset', 'address')).toContain('required');
	});

	it('says unchanged for a write that changes nothing', async () => {
		const { out, failed } = await task('node', 'patch', 'tyo', '--set', 'retired=false');
		expect(failed).toBeUndefined();
		expect(out).toContain(`node ${NODE_ID.slice(0, 8)}: unchanged`);
	});

	it('fails a refusal with its code and message', async () => {
		state.refusal = ['forbidden_field', '`status` is not written'];
		const { failed } = await task('node', 'patch', 'tyo', '--set', 'retired=true');
		expect(failed).toContain('refused forbidden_field: `status` is not written');
	});

	it('sends nothing without the write token', async () => {
		token = '';
		expect(await refused('app', 'create', 'web', '--set', 'placements=tyo')).toContain(
			'CONTROL_WRITE_TOKEN',
		);
	});

	it('fails a patch with nothing to change', async () => {
		expect(await refused('node', 'patch', 'tyo')).toContain('nothing to change');
	});
});

describe('masked values', () => {
	it('are given like any value, never asked for', async () => {
		const items = ['--identity', `email,value=${SECRET_VALUE}`, '--identity', 'account_id,value=t'];
		const { out, failed } = await task('account', 'create', '--set', 'provider=azure', ...items);
		expect(failed).toBeUndefined();
		expect(asked).toEqual([]);
		const sent = [
			{ kind: 'email', value: SECRET_VALUE },
			{ kind: 'account_id', value: 't' },
		];
		const [made] = writes();
		expect([made!.method, made!.path]).toEqual(['POST', '/v1/registry/accounts']);
		expect(made!.body!.spec.identities).toEqual(sent);
		expect(out).toContain('new account: written, id 0000000b00000000, revision 11');
	});

	it('are made with no name to give', async () => {
		expect(await refused('account', 'create', 'az', '--set', 'provider=azure')).toContain('usage');
	});

	it('are required where the catalog says', async () => {
		const failed = await refused('account', 'patch', ACCOUNT_ID.slice(0, 8), '--identity', 'email');
		expect(failed).toContain('takes its value');
	});

	it('replace the item of their key on a patch and keep the rest', async () => {
		const { failed } = await task(
			'account',
			'patch',
			ACCOUNT_ID.slice(0, 8),
			'--identity',
			'email,value=new',
		);
		expect(failed).toBeUndefined();
		const sent = [IDENTITIES[0], { kind: 'email', value: 'new' }];
		expect(writes()[0]!.body!.spec.identities).toEqual(sent);
	});

	it('print as dots until --reveal, and as answered with --json', async () => {
		const hidden = await task('account', 'get', ACCOUNT_ID.slice(0, 8));
		expect(hidden.out).not.toContain(SECRET_VALUE);
		expect(hidden.out).toContain('kind=email  value=••••••••');
		printed = [];
		const revealed = await task('account', 'get', ACCOUNT_ID.slice(0, 8), '--reveal');
		expect(revealed.out).toContain(`kind=email  value=${SECRET_VALUE}`);
		printed = [];
		const raw = await task('account', 'get', ACCOUNT_ID.slice(0, 8), '--json');
		expect(JSON.parse(raw.out).spec.identities).toEqual(IDENTITIES);
	});

	it('print as dots in a list column until --reveal', () => {
		const field = { name: 'value', type: 'string', masked: true, editable: true, required: true };
		expect(cell(SECRET_VALUE, field as Field)).toBe('••••••••');
		expect(cell(SECRET_VALUE, field as Field, true)).toBe(SECRET_VALUE);
		expect(cell(undefined, field as Field)).toBe('-');
	});
});

describe('secrets', () => {
	beforeEach(() => {
		state.catalog = WITH_A_SECRET;
		state.held.accounts![ACCOUNT_ID]!.spec.identities = [
			{ kind: 'username', secret: 'accounts/registry/ACCOUNT_00000000000000A1_USERNAME' },
			{ kind: 'email', secret: 'accounts/registry/ACCOUNT_00000000000000A1_EMAIL' },
		];
	});

	it("types in an item's secret once per item, and sends it", async () => {
		const items = ['--identity', 'email', '--identity', 'account_id'];
		const { out, failed } = await task('account', 'create', '--set', 'provider=azure', ...items);
		expect(failed, out).toBeUndefined();
		expect(asked).toEqual(['identity email: value: ', 'identity account_id: value: ']);
		const sent = ['email', 'account_id'].map((kind) => ({ kind, value: SECRET_VALUE }));
		expect(writes()[0]!.body!.spec.identities).toEqual(sent);
		expect(out).not.toContain(SECRET_VALUE);
	});

	it('replaces the item of its key on a patch and keeps the rest', async () => {
		const { failed } = await task(
			'account',
			'patch',
			ACCOUNT_ID.slice(0, 8),
			'--identity',
			'email',
		);
		expect(failed).toBeUndefined();
		const identities = writes()[0]!.body!.spec.identities as Record<string, unknown>[];
		expect(identities.map((one) => one.kind)).toEqual(['username', 'email']);
		expect(identities[0]!.secret).toBe('accounts/registry/ACCOUNT_00000000000000A1_USERNAME');
		expect(identities[1]).toEqual({ kind: 'email', value: SECRET_VALUE });
	});

	it.each([
		['--identity', `email,value=${SECRET_VALUE}`],
		['--json', `identities=[{"kind": "email", "value": "${SECRET_VALUE}"}]`],
	])('never takes a secret from %s', async (option, text) => {
		const failed = await refused('account', 'patch', ACCOUNT_ID.slice(0, 8), option, text);
		expect(failed).toContain('never taken from an argument');
		expect(failed).not.toContain(SECRET_VALUE);
		expect(asked).toEqual([]);
	});

	it('refuses what control writes', async () => {
		const failed = await refused(
			'account',
			'patch',
			ACCOUNT_ID.slice(0, 8),
			'--identity',
			'email,secret=a/b/C',
		);
		expect(failed).toContain('not editable');
	});

	it("holds an item's key to its choices", async () => {
		expect(
			await refused('account', 'patch', ACCOUNT_ID.slice(0, 8), '--identity', 'password'),
		).toContain('one of');
	});

	it('asks for nothing on a dry run, sends nothing, and hides every secret', async () => {
		const given = ['--set', 'provider=azure', '--identity', 'email', '--dry-run'];
		const { out, failed } = await task('account', 'create', ...given);
		expect(failed).toBeUndefined();
		expect([asked, writes()]).toEqual([[], []]);
		expect(out).toContain('would POST /v1/registry/accounts,');
		expect(shown(out).spec.identities).toEqual([{ kind: 'email', value: '<hidden>' }]);
		expect(out).not.toContain(TOKEN);
		expect(out).not.toContain(SECRET_VALUE);
	});

	it('reads the revision on a patch dry run and writes nothing', async () => {
		const { out } = await task('node', 'patch', 'tyo', '--set', 'retired=true', '-n');
		expect(writes()).toEqual([]);
		expect(out).toContain(`would PATCH /v1/registry/nodes/${NODE_ID}`);
		expect(shown(out).revision).toBe(10);
	});
});

describe('the catalog', () => {
	it("is refused where it is not the schema's, before anything reads it", async () => {
		CATALOG.push({ name: 'node', fields: [], actions: [], columns: [] } as unknown as Description);
		try {
			expect(await refused('node', 'list')).toContain('catalog.schema.json');
		} finally {
			CATALOG.pop();
		}
	});

	const examples = (which: string) =>
		readdirSync(join(EXAMPLES, which)).map((file) => [
			file,
			JSON.parse(readFileSync(join(EXAMPLES, which, file), 'utf8')) as unknown,
		]);

	it.each(examples('valid'))('takes the shared valid example %s', (_, example) => {
		expect(v.safeParse(Catalog, example).success).toBe(true);
	});

	it.each(examples('invalid'))('refuses the shared invalid example %s', (_, example) => {
		expect(v.safeParse(Catalog, example).success).toBe(false);
	});
});

describe('the edge', () => {
	it('reads each node id by its name from nodes.toml, and refuses one without', () => {
		const ids = nodeIds(`[tyo]\nid = "${NODE_ID}"\ncity = "Tokyo"\n\n[sin]\nid = "${SIN_ID}"\n`);
		expect(ids).toEqual(IDS);
		expect(() => nodeIds('[tyo]\ncity = "Tokyo"\n')).toThrow(Failure);
		expect(() => nodeIds('[tyo]\nid = "D132B0F553E3DCC3"\n')).toThrow(Failure);
	});

	it('names the nodes control places an app on, in its order, by the read token', async () => {
		const app = (placements: string[]) =>
			state.resource('app', `${placements.length}`.padStart(16, '0'), 'secrets', { placements });
		state.held.apps!.secrets = app([SIN_ID, NODE_ID]);
		expect(await placedOn('secrets', world())).toEqual(['sin', 'tyo']);
		expect(state.requests.map((sent) => [sent.path, sent.auth])).toEqual([
			['/v1/registry/apps/secrets', `Bearer ${READ}`],
		]);
		state.held.apps!.secrets = app([]);
		expect(await placedOn('secrets', world())).toEqual([]);
		state.held.apps!.secrets = app(['0000000000000000']);
		await expect(placedOn('secrets', world())).rejects.toThrow('nodes.toml names no node');
	});
});

describe('naming', () => {
	it("names an item by its list's singular", () => {
		expect(['identities', 'records', 'data'].map(singular)).toEqual(['identity', 'record', 'data']);
	});
});
