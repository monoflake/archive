import { readdirSync, readFileSync } from 'node:fs';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { join } from 'node:path';
import * as v from 'valibot';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { Catalog, type Description, type Field } from './catalog.gen.ts';
import { Failure, cell, run, singular, type World } from './platform.ts';

const REGISTRY = join(import.meta.dirname, '..');
// The fake answers the catalog the registry generates, never a copy of it -- RFC bq3vn.
const CATALOG = JSON.parse(readFileSync(join(REGISTRY, 'catalog.json'), 'utf8')) as Description[];
const EXAMPLES = join(REGISTRY, 'examples/catalog');

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
									{ name: 'value', type: 'string' as const, secret: true, editable: true, required: false },
									{ name: 'secret', type: 'string' as const, editable: false, required: false },
								],
							},
				),
			},
);

const TOKEN = 'token-for-the-fake';
const SECRET_VALUE = 'me-at-example-dot-com';
const DOMAIN_ID = '00000000000000d1';
/** An account's identities as the registry keeps and answers them. */
const IDENTITIES = [
	{ kind: 'user_id', value: 'ocid1.user.oc1..example' },
	{ kind: 'email', value: SECRET_VALUE },
];
const NODE_ID = 'abd3922110a78032';

interface Resource {
	kind: string;
	id: string;
	name: string;
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
class Registry {
	revision = 10;
	requests: Sent[] = [];
	refusal: [string, string] | undefined;
	catalog: Description[] = CATALOG;
	held: Record<string, Record<string, Resource>>;

	constructor() {
		this.held = {
			domains: { oci: this.resource('domain', DOMAIN_ID, 'oci', {}) },
			nodes: {
				tyo: this.resource('node', NODE_ID, 'tyo', {
					address: '100.64.0.1',
					tier: 'datacenter',
					domain: DOMAIN_ID,
					system: 'debian',
					retired: false,
					roles: ['compute', 'core'],
					grants: ['hub:peer'],
				}),
			},
			apps: {},
			accounts: {
				'oci-2': this.resource('account', '00000000000000a1', 'oci-2', {
					provider: 'oracle',
					identities: structuredClone(IDENTITIES),
				}),
			},
		};
	}

	resource(kind: string, id: string, name: string, spec: Record<string, unknown>): Resource {
		return { kind, id, name, revision: this.revision, spec };
	}

	find(plural: string, key: string): Resource | undefined {
		return Object.values(this.held[plural] ?? {}).find((one) => key === one.id || key === one.name);
	}

	answer(sent: Sent): [number, unknown] {
		this.requests.push(sent);
		const parts = sent.path.replace(/^\/|\/$/g, '').split('/');
		if (parts.join('/') === 'v1/kinds') {
			return [200, { status: 'success', data: { items: this.catalog } }];
		}
		const plural = parts[1] ?? '';
		const held = this.held[plural];
		if (held === undefined) {
			return [404, { status: 'error', code: 'no_such_kind', message: 'No such kind' }];
		}
		if (parts.length === 2) {
			const items = Object.values(held);
			return [200, { status: 'success', data: { items, revision: this.revision } }];
		}
		const key = parts[2] ?? '';
		const found = this.find(plural, key);
		if (sent.method === 'GET') {
			if (found === undefined) {
				return [404, { status: 'error', code: 'no_such_resource', message: 'None held' }];
			}
			return [200, { status: 'success', data: found }];
		}
		if (sent.auth !== `Bearer ${TOKEN}`) {
			return [401, { status: 'error', code: 'unauthorized', message: 'No token' }];
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
		const made = found ?? this.resource(kind, this.revision.toString(16).padStart(16, '0'), key, {});
		made.spec = spec;
		made.revision = this.revision;
		held[made.name] = made;
		const data = { revision: this.revision, task: '00000000000000f1' };
		return [202, { status: 'success', data }];
	}
}

let state: Registry;
let server: Server;
let printed: string[];
let asked: string[];
let token: string | undefined;

beforeEach(async () => {
	state = new Registry();
	printed = [];
	asked = [];
	token = TOKEN;
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

function world(): World {
	return {
		placements: () => ['tyo', 'sin', 'rdu'],
		address: () => '127.0.0.1',
		port: (server.address() as AddressInfo).port,
		token: () => token,
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

const writes = () => state.requests.filter((sent) => sent.method === 'PUT' || sent.method === 'PATCH');

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
	it('lists the catalog columns, ids by their first eight digits, with no token', async () => {
		const { out, failed } = await task('nodes', 'list');
		expect(failed).toBeUndefined();
		const [head, row] = out.split('\n');
		expect(head!.split(/\s+/)).toEqual(['id', 'name', 'address', 'tier', 'domain', 'system']);
		expect(row!.split(/\s+/)).toEqual([
			NODE_ID.slice(0, 8),
			'tyo',
			'100.64.0.1',
			'datacenter',
			DOMAIN_ID.slice(0, 8),
			'debian',
		]);
		expect(state.requests.every((sent) => sent.auth === undefined)).toBe(true);
	});

	it('lists the items as answered with --json', async () => {
		const { out } = await task('node', 'list', '--json');
		expect(JSON.parse(out)).toEqual([state.held.nodes!.tyo]);
	});

	it("gets the spec in the catalog's order, then the rest", async () => {
		const { out } = await task('node', 'get', 'tyo');
		const lines = out.split('\n');
		expect(lines[0]).toBe('node tyo');
		expect(lines[1], 'get shows the id whole').toContain(NODE_ID);
		const names = lines.slice(3).map((line) => line.trim().split(/\s+/)[0]);
		expect(names).toEqual(['address', 'tier', 'domain', 'system', 'retired', 'roles', 'grants']);
		expect(out).toContain('compute,core');
	});

	it('gets by id, as JSON', async () => {
		const { out } = await task('node', 'get', NODE_ID, '--json');
		expect(JSON.parse(out).name).toBe('tyo');
	});

	it('fails on a kind the catalog does not hold, naming those it does', async () => {
		expect(await refused('widgets', 'list')).toContain('node (nodes)');
	});

	it('fails on a resource not held, with its code', async () => {
		expect(await refused('node', 'get', 'zzz')).toContain('no_such_resource');
	});

	it('refuses an option the catalog does not make', async () => {
		expect(await refused('domain', 'create', 'home', '--identity', 'email')).toContain(
			'--identity',
		);
	});
});

describe('writing', () => {
	it('creates by PUT at revision 0, as operator, with the token', async () => {
		const { out, failed } = await task('app', 'create', 'ledger', '--set', 'placements=tyo,sin');
		expect(failed).toBeUndefined();
		const [sent] = writes();
		expect([sent!.method, sent!.path]).toEqual(['PUT', '/v1/apps/ledger']);
		expect(sent!.auth).toBe(`Bearer ${TOKEN}`);
		const spec = { placements: ['tyo', 'sin'] };
		expect(sent!.body).toEqual({ by: 'operator', revision: 0, spec });
		expect(out).toContain('written, revision 11, task 00000000000000f1');
	});

	it('resolves a reference by name and types every scalar', async () => {
		const scalars = ['address=100.64.0.2', 'tier=datacenter', 'domain=oci', 'system=alpine'];
		const set = [...scalars, 'expiry=2036', 'core=true'].flatMap((pair) => ['--set', pair]);
		const { failed } = await task('node', 'create', 'sin', ...set, '--json', 'grants=["hub:peer"]');
		expect(failed).toBeUndefined();
		const spec = writes()[0]!.body!.spec;
		expect(spec.domain).toBe(DOMAIN_ID);
		expect([spec.expiry, spec.core, spec.grants]).toEqual([2036, true, ['hub:peer']]);
	});

	it('fails a create without a required field', async () => {
		expect(await refused('app', 'create', 'ledger')).toContain('placements is required');
	});

	it("fails a create of a name of an id's shape", async () => {
		await refused('domain', 'create', '0123456789abcdef');
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
		expect([sent!.method, sent!.path]).toEqual(['PATCH', `/v1/nodes/${NODE_ID}`]);
		const spec = { retired: true, roles: null };
		expect(sent!.body).toEqual({ by: 'operator', revision: 10, spec });
		expect(state.held.nodes!.tyo!.spec).not.toHaveProperty('roles');
	});

	it('never unsets a required field', async () => {
		expect(await refused('node', 'patch', 'tyo', '--unset', 'address')).toContain('required');
	});

	it('says unchanged for a write that changes nothing', async () => {
		const { out, failed } = await task('node', 'patch', 'tyo', '--set', 'retired=false');
		expect(failed).toBeUndefined();
		expect(out).toContain('node tyo: unchanged');
	});

	it('fails a refusal with its code and message', async () => {
		state.refusal = ['forbidden_field', '`status` is not written'];
		const { failed } = await task('node', 'patch', 'tyo', '--set', 'retired=true');
		expect(failed).toContain('refused forbidden_field: `status` is not written');
	});

	it('sends nothing without the token', async () => {
		token = '';
		expect(await refused('domain', 'create', 'home')).toContain('REGISTRY_TOKEN');
	});

	it('fails a patch with nothing to change', async () => {
		expect(await refused('node', 'patch', 'tyo')).toContain('nothing to change');
	});
});

describe('masked values', () => {
	it('are given like any value, never asked for', async () => {
		const items = ['--identity', `email,value=${SECRET_VALUE}`, '--identity', 'tenancy,value=t'];
		const { failed } = await task('account', 'create', 'az', '--set', 'provider=azure', ...items);
		expect(failed).toBeUndefined();
		expect(asked).toEqual([]);
		const sent = [
			{ kind: 'email', value: SECRET_VALUE },
			{ kind: 'tenancy', value: 't' },
		];
		expect(writes()[0]!.body!.spec.identities).toEqual(sent);
	});

	it('are required where the catalog says', async () => {
		const failed = await refused('account', 'patch', 'oci-2', '--identity', 'email');
		expect(failed).toContain('takes its value');
	});

	it('replace the item of their key on a patch and keep the rest', async () => {
		const { failed } = await task('account', 'patch', 'oci-2', '--identity', 'email,value=new');
		expect(failed).toBeUndefined();
		const sent = [IDENTITIES[0], { kind: 'email', value: 'new' }];
		expect(writes()[0]!.body!.spec.identities).toEqual(sent);
	});

	it('print as dots until --reveal, and as answered with --json', async () => {
		const hidden = await task('account', 'get', 'oci-2');
		expect(hidden.out).not.toContain(SECRET_VALUE);
		expect(hidden.out).toContain('kind=email  value=••••••••');
		printed = [];
		const revealed = await task('account', 'get', 'oci-2', '--reveal');
		expect(revealed.out).toContain(`kind=email  value=${SECRET_VALUE}`);
		printed = [];
		const raw = await task('account', 'get', 'oci-2', '--json');
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
		state.held.accounts!['oci-2']!.spec.identities = [
			{ kind: 'user_id', secret: 'accounts/registry/ACCOUNT_00000000000000A1_USER_ID' },
			{ kind: 'email', secret: 'accounts/registry/ACCOUNT_00000000000000A1_EMAIL' },
		];
	});

	it("types in an item's secret once per item, and sends it", async () => {
		const items = ['--identity', 'email', '--identity', 'tenancy'];
		const { out, failed } = await task('account', 'create', 'az', '--set', 'provider=azure', ...items);
		expect(failed, out).toBeUndefined();
		expect(asked).toEqual(['identity email: value: ', 'identity tenancy: value: ']);
		const sent = ['email', 'tenancy'].map((kind) => ({ kind, value: SECRET_VALUE }));
		expect(writes()[0]!.body!.spec.identities).toEqual(sent);
		expect(out).not.toContain(SECRET_VALUE);
	});

	it('replaces the item of its key on a patch and keeps the rest', async () => {
		const { failed } = await task('account', 'patch', 'oci-2', '--identity', 'email');
		expect(failed).toBeUndefined();
		const identities = writes()[0]!.body!.spec.identities as Record<string, unknown>[];
		expect(identities.map((one) => one.kind)).toEqual(['user_id', 'email']);
		expect(identities[0]!.secret).toBe('accounts/registry/ACCOUNT_00000000000000A1_USER_ID');
		expect(identities[1]).toEqual({ kind: 'email', value: SECRET_VALUE });
	});

	it.each([
		['--identity', `email,value=${SECRET_VALUE}`],
		['--json', `identities=[{"kind": "email", "value": "${SECRET_VALUE}"}]`],
	])('never takes a secret from %s', async (option, text) => {
		const failed = await refused('account', 'patch', 'oci-2', option, text);
		expect(failed).toContain('never taken from an argument');
		expect(failed).not.toContain(SECRET_VALUE);
		expect(asked).toEqual([]);
	});

	it('refuses what the registry writes', async () => {
		const failed = await refused('account', 'patch', 'oci-2', '--identity', 'email,secret=a/b/C');
		expect(failed).toContain('not editable');
	});

	it("holds an item's key to its choices", async () => {
		expect(await refused('account', 'patch', 'oci-2', '--identity', 'password')).toContain('one of');
	});

	it('asks for nothing on a dry run, sends nothing, and hides every secret', async () => {
		const given = ['--set', 'provider=azure', '--identity', 'email', '--dry-run'];
		const { out, failed } = await task('account', 'create', 'az', ...given);
		expect(failed).toBeUndefined();
		expect([asked, writes()]).toEqual([[], []]);
		expect(out).toContain('would PUT /v1/accounts/az');
		expect(shown(out).spec.identities).toEqual([{ kind: 'email', value: '<hidden>' }]);
		expect(out).not.toContain(TOKEN);
		expect(out).not.toContain(SECRET_VALUE);
	});

	it('reads the revision on a patch dry run and writes nothing', async () => {
		const { out } = await task('node', 'patch', 'tyo', '--set', 'retired=true', '-n');
		expect(writes()).toEqual([]);
		expect(out).toContain(`would PATCH /v1/nodes/${NODE_ID}`);
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

describe('naming', () => {
	it("names an item by its list's singular", () => {
		expect(['identities', 'records', 'data'].map(singular)).toEqual(['identity', 'record', 'data']);
	});
});
