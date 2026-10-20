import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { canonical } from './import.ts';
import { clashed, committed, set } from './store.ts';
import {
	body,
	CDN,
	control,
	GEO,
	IMPORT,
	imported,
	ORACLE,
	READ,
	refused,
	shown,
	SIN,
	Sqlite,
	TYO,
	WEB,
	WRITE,
} from './testing.ts';

afterEach(() => {
	vi.restoreAllMocks();
});

const RELEASE = {
	repository: 'monoflake/platform',
	run: 7,
	commit: '0123456789abcdef0123456789abcdef01234567',
};

function landed(revision: number, id?: string) {
	const data = { revision, task: expect.stringMatching(/^[0-9a-f]{16}$/) };
	return { status: 202, body: { status: 'success', data: id ? { id, ...data } : data } };
}

const UNCHANGED = { status: 200, body: { status: 'success', data: {} } };

/** control holding the import, and a write as `by: test` with the write token. */
async function writable() {
	const { db, call } = await imported();
	const write = (method: string, path: string, revision: number, spec: unknown) =>
		call(method, path, { token: WRITE, body: { by: 'test', revision, spec } });
	const get = async (path: string) => (await call('GET', path, { token: READ })).body.data;
	const changes = () => db.db.prepare('SELECT id, by, kind, resource, name FROM changes').all();
	return { db, call, write, get, changes };
}

describe('create', () => {
	it('makes a node at revision 0, its id drawn and answered, and a change naming who', async () => {
		const { write, get, changes } = await writable();
		const spec = shown('100.64.0.3', 'Ashburn', 'US');
		const made = await write('POST', '/v1/registry/nodes', 0, spec);
		expect(made).toEqual(landed(42, expect.stringMatching(/^[0-9a-f]{16}$/)));
		const id = (made.body.data as { id: string }).id;
		expect(await get(`/v1/registry/nodes/${id}`)).toEqual({
			kind: 'node',
			id,
			name: null,
			revision: 42,
			spec,
		});
		expect(changes()).toContainEqual({
			id: 42,
			by: 'test',
			kind: 'node',
			resource: id,
			name: null,
		});
	});

	it('makes an account, and refuses one at any revision but 0', async () => {
		const { write } = await writable();
		const spec = { provider: 'azure', identities: [{ kind: 'username', value: 'me' }] };
		expect(await write('POST', '/v1/registry/accounts', 0, spec)).toEqual(
			landed(42, expect.any(String)),
		);
		const other = { provider: 'azure', identities: [{ kind: 'username', value: 'you' }] };
		expect(await write('POST', '/v1/registry/accounts', 3, other)).toEqual(
			refused(409, 'invalid_revision'),
		);
	});

	it('makes an app by its name alone', async () => {
		const { write } = await writable();
		const answer = await write('POST', '/v1/registry/apps', 0, { placements: [] });
		expect(answer).toEqual(refused(405, 'invalid_method'));
	});

	it('draws an id again where the one drawn is held', async () => {
		const { write } = await writable();
		const held = Uint8Array.from(TYO.match(/../g) ?? [], (pair) => Number.parseInt(pair, 16));
		const draw = vi.spyOn(crypto, 'getRandomValues');
		draw.mockImplementationOnce((array) => {
			(array as Uint8Array).set(held);
			return array;
		});
		const made = await write('POST', '/v1/registry/nodes', 0, shown('100.64.0.3', 'Ashburn', 'US'));
		expect(made.status).toBe(202);
		expect((made.body.data as { id: string }).id).not.toBe(TYO);
		expect(draw.mock.calls.length).toBeGreaterThan(2);
	});
});

describe('put', () => {
	it('makes an app at revision 0 by its name, then replaces its whole spec', async () => {
		const { write, get } = await writable();
		const spec = { placements: [TYO], display_name: 'API' };
		expect(await write('PUT', '/v1/registry/apps/api', 0, spec)).toEqual(landed(42));
		expect(await get('/v1/registry/apps/api')).toMatchObject({ name: 'api', revision: 42, spec });
		expect(await write('PUT', '/v1/registry/apps/api', 42, { placements: [SIN, TYO] })).toEqual(
			landed(43),
		);
		expect(await get('/v1/registry/apps/api')).toMatchObject({ spec: { placements: [SIN, TYO] } });
	});

	it('replaces an app named by its id, and refuses an id it does not hold', async () => {
		const { write, get } = await writable();
		expect(await write('PUT', `/v1/registry/apps/${WEB}`, 30, { placements: [SIN] })).toEqual(
			landed(42),
		);
		expect(await get('/v1/registry/apps/web')).toMatchObject({ spec: { placements: [SIN] } });
		const absent = await write('PUT', '/v1/registry/apps/ffffffffffffffff', 0, { placements: [] });
		expect(absent).toEqual(refused(404, 'no_such_resource'));
	});

	it('writes nothing, and answers no revision, when nothing changes', async () => {
		const { write, changes } = await writable();
		const before = changes();
		expect(await write('PUT', '/v1/registry/apps/geo', 40, { placements: [TYO, SIN] })).toEqual(
			UNCHANGED,
		);
		expect(changes()).toEqual(before);
	});

	it('refuses a name that is no DNS label, and a kind made otherwise', async () => {
		const { write } = await writable();
		expect(await write('PUT', '/v1/registry/apps/Web', 0, { placements: [] })).toEqual(
			refused(400, 'invalid_name'),
		);
		expect(
			await write('PUT', `/v1/registry/nodes/${TYO}`, 11, shown('1.1.1.1', 'Tokyo', 'JP')),
		).toEqual(refused(405, 'invalid_method'));
	});
});

describe('patch', () => {
	it('lays the fields given over the spec kept', async () => {
		const { write, get } = await writable();
		expect(await write('PATCH', '/v1/registry/apps/web', 30, { release: RELEASE })).toEqual(
			landed(42),
		);
		expect(await get('/v1/registry/apps/web')).toMatchObject({
			revision: 42,
			spec: { placements: [TYO], release: RELEASE },
		});
	});

	it('takes out a key given null, and merges an object given into the one kept', async () => {
		const { write, get } = await writable();
		const release = { ...RELEASE, run: 8 };
		expect(
			await write('PATCH', `/v1/registry/apps/${CDN}`, 41, { display_name: null, release }),
		).toEqual(landed(42));
		expect(await write('PATCH', '/v1/registry/apps/cdn', 42, { release: { run: 9 } })).toEqual(
			landed(43),
		);
		expect(await get('/v1/registry/apps/cdn')).toMatchObject({
			spec: { placements: [], release: { ...RELEASE, run: 9 } },
		});
		expect(((await get('/v1/registry/apps/cdn')) as { spec: object }).spec).not.toHaveProperty(
			'display_name',
		);
	});

	it('changes a node by its id, and an account', async () => {
		const { write, get } = await writable();
		expect(await write('PATCH', `/v1/registry/nodes/${TYO}`, 11, { retired: true })).toEqual(
			landed(42),
		);
		expect(await get(`/v1/registry/nodes/${TYO}`)).toMatchObject({
			spec: { retired: true, city: 'Tokyo' },
		});
		const identities = [{ kind: 'email', value: 'A@example.com' }];
		expect(await write('PATCH', `/v1/registry/accounts/${ORACLE}`, 5, { identities })).toEqual(
			landed(43),
		);
	});

	it('writes nothing when the merge changes nothing', async () => {
		const { write, changes } = await writable();
		const before = changes();
		expect(await write('PATCH', '/v1/registry/apps/web', 30, { placements: [TYO] })).toEqual(
			UNCHANGED,
		);
		expect(await write('PATCH', '/v1/registry/apps/web', 30, { display_name: null })).toEqual(
			UNCHANGED,
		);
		expect(changes()).toEqual(before);
	});

	it('refuses what it does not hold, a name for a kind without one, and a spec not an object', async () => {
		const { write } = await writable();
		expect(await write('PATCH', '/v1/registry/nodes/ffffffffffffffff', 0, {})).toEqual(
			refused(404, 'no_such_node'),
		);
		expect(await write('PATCH', '/v1/registry/apps/nothing', 0, {})).toEqual(
			refused(404, 'no_such_resource'),
		);
		expect(await write('PATCH', '/v1/registry/nodes/tyo', 11, {})).toEqual(
			refused(400, 'invalid_name'),
		);
		expect(await write('PATCH', '/v1/registry/accounts/oracle', 5, {})).toEqual(
			refused(400, 'invalid_name'),
		);
		expect(await write('PATCH', '/v1/registry/apps/web', 30, [])).toEqual(
			refused(400, 'invalid_body'),
		);
		expect(await write('PATCH', '/v1/registry/things/web', 30, {})).toEqual(
			refused(404, 'no_such_kind'),
		);
	});

	it('checks the spec the merge makes whole', async () => {
		const { write } = await writable();
		const answer = await write('PATCH', '/v1/registry/apps/web', 30, { placements: null });
		expect(answer).toEqual(refused(400, 'invalid_body'));
		const node = await write('PATCH', `/v1/registry/nodes/${TYO}`, 11, { continent: 'XX' });
		expect(node).toEqual(refused(400, 'invalid_body'));
	});
});

/** GEO's spec as a write read at its revision makes it. */
function geo(spec: object) {
	return {
		kind: 'app' as const,
		id: GEO,
		name: 'geo',
		made: false,
		revision: 40,
		spec: canonical(spec),
		by: 'test',
		at: 'now',
	};
}

describe('revisions', () => {
	it('refuses a write held to a revision since passed, and writes nothing', async () => {
		const { write, get } = await writable();
		expect(await write('PATCH', '/v1/registry/apps/web', 30, { display_name: 'Web' })).toEqual(
			landed(42),
		);
		expect(await write('PATCH', '/v1/registry/apps/web', 30, { display_name: 'Site' })).toEqual(
			refused(409, 'invalid_revision'),
		);
		expect(await write('PUT', '/v1/registry/apps/web', 30, { placements: [] })).toEqual(
			refused(409, 'invalid_revision'),
		);
		expect(await write('PUT', '/v1/registry/apps/web', 0, { placements: [] })).toEqual(
			refused(409, 'invalid_revision'),
		);
		expect(await write('PUT', '/v1/registry/apps/new', 7, { placements: [] })).toEqual(
			refused(409, 'invalid_revision'),
		);
		expect(await get('/v1/registry/apps/web')).toMatchObject({
			revision: 42,
			spec: { display_name: 'Web' },
		});
	});

	it('lands one of two batches held to one revision, and the other not at all', async () => {
		const { db, get } = await writable();
		expect(await set(db, geo({ placements: [TYO] }), 'aaaaaaaaaaaaaaaa')).toBe(42);
		expect(await set(db, geo({ placements: [SIN] }), 'bbbbbbbbbbbbbbbb')).toBeUndefined();
		expect(await get('/v1/registry/apps/geo')).toMatchObject({
			revision: 42,
			spec: { placements: [TYO] },
		});
		const tasks = db.db.prepare('SELECT task FROM changes WHERE task IS NOT NULL').all();
		expect(tasks).toEqual([{ task: 'aaaaaaaaaaaaaaaa' }]);
	});

	it('answers two writes sent at once with one revision and one conflict', async () => {
		const { write } = await writable();
		const answers = await Promise.all([
			write('PATCH', '/v1/registry/apps/geo', 40, { display_name: 'Geo' }),
			write('PATCH', '/v1/registry/apps/geo', 40, { display_name: 'GeoIP' }),
		]);
		expect(answers.map((answer) => answer.status).toSorted()).toEqual([202, 409]);
	});

	it('makes one app of two made at once by one name', async () => {
		const { write } = await writable();
		const answers = await Promise.all([
			write('PUT', '/v1/registry/apps/api', 0, { placements: [TYO] }),
			write('PUT', '/v1/registry/apps/api', 0, { placements: [SIN] }),
		]);
		expect(answers.map((answer) => answer.status).toSorted()).toEqual([202, 409]);
	});
});

describe('placements', () => {
	it('refuses an id that is no node control holds, or one retired', async () => {
		const { write } = await writable();
		const placed = (placements: unknown[]) =>
			write('PUT', '/v1/registry/apps/api', 0, { placements });
		expect(await placed(['ffffffffffffffff'])).toEqual(refused(400, 'invalid_resource'));
		expect(await placed([ORACLE])).toEqual(refused(400, 'invalid_resource'));
		expect(await write('PATCH', `/v1/registry/nodes/${SIN}`, 12, { retired: true })).toEqual(
			landed(42),
		);
		expect(await placed([TYO, SIN])).toEqual(refused(400, 'invalid_resource'));
		expect(await placed([TYO])).toEqual(landed(43));
	});

	it('keeps an app on a node retired since it was placed there', async () => {
		const { write } = await writable();
		expect(await write('PATCH', `/v1/registry/nodes/${SIN}`, 12, { retired: true })).toEqual(
			landed(42),
		);
		expect(await write('PATCH', '/v1/registry/apps/geo', 40, { release: RELEASE })).toEqual(
			landed(43),
		);
		expect(await write('PUT', '/v1/registry/apps/geo', 43, { placements: [SIN] })).toEqual(
			landed(44),
		);
		expect(await write('PATCH', '/v1/registry/apps/web', 30, { placements: [TYO, SIN] })).toEqual(
			refused(400, 'invalid_resource'),
		);
	});

	it('refuses a placement that is not an id, or one named twice', async () => {
		const { write } = await writable();
		const cases = [['tyo'], [TYO, TYO], [1], 'tyo'];
		const answers = await Promise.all(
			cases.map((placements) => write('PUT', '/v1/registry/apps/api', 0, { placements })),
		);
		for (const [index, answer] of answers.entries()) {
			expect(answer, JSON.stringify(cases[index])).toEqual(refused(400, 'invalid_body'));
		}
	});

	it('refuses a node naming an account control does not hold', async () => {
		const { write } = await writable();
		const missing = await write('PATCH', `/v1/registry/nodes/${TYO}`, 11, {
			account: 'ffffffffffffffff',
		});
		expect(missing).toEqual(refused(400, 'invalid_resource'));
		expect(await write('PATCH', `/v1/registry/nodes/${TYO}`, 11, { account: ORACLE })).toEqual(
			landed(42),
		);
	});
});

describe('forbidden fields', () => {
	it('refuses `status` and `drift` on every kind, and `name` on a kind without one', async () => {
		const { call } = await writable();
		const sent = (method: string, path: string, extra: object) =>
			call(method, path, { token: WRITE, body: { by: 'test', revision: 30, spec: {}, ...extra } });
		const paths = [
			'/v1/registry/apps/web',
			`/v1/registry/nodes/${TYO}`,
			`/v1/registry/accounts/${ORACLE}`,
		];
		const cases = paths.flatMap((path) => ['status', 'drift'].map((key) => ({ path, key })));
		const answers = await Promise.all(
			cases.map(({ path, key }) => sent('PATCH', path, { [key]: {} })),
		);
		for (const [index, answer] of answers.entries()) {
			expect(answer, JSON.stringify(cases[index])).toEqual(refused(400, 'forbidden_field'));
		}
		expect(await sent('PATCH', `/v1/registry/nodes/${TYO}`, { name: 'tyo' })).toEqual(
			refused(400, 'forbidden_field'),
		);
		expect(await sent('POST', '/v1/registry/accounts', { name: 'oci' })).toEqual(
			refused(400, 'forbidden_field'),
		);
		expect((await sent('PATCH', '/v1/registry/apps/web', { name: 'web' })).status).toBe(200);
	});

	it("refuses a key an app's or an account's spec does not hold, and a node's `place`", async () => {
		const { write } = await writable();
		expect(await write('PATCH', '/v1/registry/apps/web', 30, { image: 'web:1' })).toEqual(
			refused(400, 'forbidden_field'),
		);
		expect(await write('PATCH', `/v1/registry/accounts/${ORACLE}`, 5, { name: 'oci' })).toEqual(
			refused(400, 'forbidden_field'),
		);
		const secret = [{ kind: 'email', value: 'a@example.com', secret: 'x' }];
		expect(
			await write('PATCH', `/v1/registry/accounts/${ORACLE}`, 5, { identities: secret }),
		).toEqual(refused(400, 'forbidden_field'));
		expect(
			await write('PATCH', `/v1/registry/nodes/${TYO}`, 11, { place: 'Tokyo, Japan' }),
		).toEqual(refused(400, 'invalid_body'));
	});

	it('refuses a body without who, the revision read or the spec', async () => {
		const { call } = await writable();
		const sent = (given: unknown) =>
			call('PATCH', '/v1/registry/apps/web', { token: WRITE, body: given });
		const cases = [
			'not a write',
			{ revision: 30, spec: {} },
			{ by: ' ', revision: 30, spec: {} },
			{ by: 'test', revision: -1, spec: {} },
			{ by: 'test', revision: 1.5, spec: {} },
			{ by: 'test', revision: 30 },
		];
		const answers = await Promise.all(cases.map(sent));
		for (const [index, answer] of answers.entries()) {
			expect(answer, JSON.stringify(cases[index])).toEqual(refused(400, 'invalid_body'));
		}
	});
});

/** An account at `provider` holding one identity. */
function at(provider: string, kind: string, value: string) {
	return { provider, identities: [{ kind, value }] };
}

describe('accounts', () => {
	it('refuses an identity another account at its provider holds, an email in any case', async () => {
		const { write } = await writable();
		const answer = await write(
			'POST',
			'/v1/registry/accounts',
			0,
			at('oracle', 'email', 'A@Example.com'),
		);
		expect(answer).toEqual(refused(400, 'invalid_resource'));
		const { message } = answer.body as { message: string };
		expect(message).toContain(ORACLE.slice(0, 8));
		expect(message.toLowerCase()).not.toContain('a@example.com');
		expect(
			(await write('POST', '/v1/registry/accounts', 0, at('azure', 'email', 'a@example.com')))
				.status,
		).toBe(202);
		expect(
			(await write('POST', '/v1/registry/accounts', 0, at('oracle', 'username', 'a@example.com')))
				.status,
		).toBe(202);
	});

	it('lets an account keep its own identities, and frees the ones it gives up', async () => {
		const { write } = await writable();
		const kept = at('oracle', 'email', 'a@example.com');
		const both = { ...kept, identities: [...kept.identities, { kind: 'username', value: 'a' }] };
		expect(await write('PATCH', `/v1/registry/accounts/${ORACLE}`, 5, both)).toEqual(landed(42));
		const given = { identities: [{ kind: 'username', value: 'a' }] };
		expect(await write('PATCH', `/v1/registry/accounts/${ORACLE}`, 42, given)).toEqual(landed(43));
		expect((await write('POST', '/v1/registry/accounts', 0, kept)).status).toBe(202);
	});

	it('refuses, in the batch itself, an identity taken after the check', async () => {
		const { db, get } = await writable();
		const change = {
			kind: 'account' as const,
			name: null,
			made: true,
			revision: 0,
			spec: canonical(at('oracle', 'email', 'a@example.com')),
			by: 'test',
			at: 'now',
			identities: [{ provider: 'oracle', kind: 'email', value: 'a@example.com' }],
		};
		const thrown = await committed(db, change).catch((error: unknown) => error);
		expect(clashed(thrown)).toBe(true);
		expect(await get('/v1/registry/accounts')).toMatchObject({
			items: [{ id: ORACLE }],
			revision: 41,
		});
	});

	it('are copied into the side table by its migration, an email in lower case', () => {
		const db = new DatabaseSync(':memory:');
		const migration = (file: string) =>
			db.exec(readFileSync(join(import.meta.dirname, '../migrations', file), 'utf8'));
		migration('0001_resources.sql');
		const spec = at('oracle', 'email', 'A@Example.com');
		db.prepare(
			"INSERT INTO resources (kind, id, spec, revision, updated_at) VALUES ('account', ?, ?, 5, 'now')",
		).run(ORACLE, canonical(spec));
		migration('0002_identities.sql');
		expect(db.prepare('SELECT provider, kind, value, account FROM identities').all()).toEqual([
			{ provider: 'oracle', kind: 'email', value: 'a@example.com', account: ORACLE },
		]);
	});

	it('is refused by an import holding one identity twice', async () => {
		const db = new Sqlite();
		const call = control(db);
		const twice = body();
		twice.accounts.push({ ...twice.accounts[0]!, id: '0000000000000d02' });
		const answer = await call('PUT', '/v1/registry/import', { token: IMPORT, body: twice });
		expect(answer).toEqual(refused(400, 'invalid_body'));
	});
});

describe('the write token', () => {
	it('is closed while it is not set, whatever the bearer', async () => {
		const call = control(new Sqlite(), { CONTROL_WRITE_TOKEN: undefined });
		const made = { by: 'test', revision: 0, spec: { placements: [] } };
		expect(await call('PUT', '/v1/registry/apps/api', { token: WRITE, body: made })).toEqual(
			refused(503, 'service_unavailable'),
		);
		expect(await call('POST', '/v1/registry/nodes', { token: '', body: made })).toEqual(
			refused(503, 'service_unavailable'),
		);
		expect((await call('GET', '/v1/registry/apps', { token: READ })).status).toBe(200);
	});

	it('reads alone while the read token is not set, and closes a read once neither is', async () => {
		const call = control(new Sqlite(), { CONTROL_READ_TOKEN: undefined });
		expect((await call('GET', '/v1/registry/apps', { token: WRITE })).status).toBe(200);
		expect(await call('GET', '/v1/registry/apps', { token: READ })).toEqual(
			refused(401, 'invalid_token'),
		);
		const closed = control(new Sqlite(), {
			CONTROL_READ_TOKEN: undefined,
			CONTROL_WRITE_TOKEN: '',
		});
		const paths = ['/v1/registry/apps', `/v1/registry/apps/${WEB}`, '/v1/registry/kinds'];
		const answers = await Promise.all(paths.map((path) => closed('GET', path, { token: WRITE })));
		for (const [index, answer] of answers.entries()) {
			expect(answer, paths[index]).toEqual(refused(503, 'service_unavailable'));
		}
	});

	it('is the one a write takes, and reads as well', async () => {
		const { call } = await writable();
		const named = { by: 'test', revision: 30, spec: { display_name: 'Web' } };
		expect(await call('PATCH', '/v1/registry/apps/web', { token: READ, body: named })).toEqual(
			refused(401, 'invalid_token'),
		);
		expect(await call('PATCH', '/v1/registry/apps/web', { body: named })).toEqual(
			refused(401, 'invalid_token'),
		);
		const read = await call('GET', '/v1/registry/apps/web', { token: WRITE });
		expect(read).toMatchObject({ status: 200, body: { data: { id: WEB, revision: 30 } } });
		expect((await call('GET', '/v1/registry/kinds', { token: WRITE })).status).toBe(200);
		expect((await call('GET', '/v1/registry/nodes', { token: READ })).status).toBe(200);
	});
});
