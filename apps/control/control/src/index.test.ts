import { describe, expect, it } from 'vitest';
import { canonical } from './import.ts';
import { app } from './index.ts';
import { type Database, replaced } from './store.ts';
import {
	body,
	control,
	GEO,
	IMPORT,
	imported,
	ORACLE,
	ORIGIN,
	READ,
	refused,
	SIN,
	Sqlite,
	TYO,
	WEB,
} from './testing.ts';

describe('health', () => {
	it('answers {} while the database answers, and 503 once it does not', async () => {
		expect(await control()('GET', '/v1/health')).toEqual({
			status: 200,
			body: { status: 'success', data: {} },
		});
		const broken: Database = {
			prepare: () => {
				throw new Error('D1 is down');
			},
			batch: () => Promise.reject(new Error('D1 is down')),
		};
		expect(await control(broken)('GET', '/v1/health')).toEqual(refused(503, 'store_unavailable'));
		const read = await control(broken)('GET', '/v1/registry/nodes', { token: READ });
		expect(read).toEqual(refused(500, 'store_unavailable'));
	});
});

describe('auth', () => {
	it('refuses a read with no bearer, with a wrong one, and with the import token', async () => {
		const call = control();
		expect(await call('GET', '/v1/registry/nodes')).toEqual(refused(401, 'invalid_token'));
		expect(await call('GET', '/v1/registry/nodes', { token: 'wrong' })).toEqual(
			refused(401, 'invalid_token'),
		);
		expect(await call('GET', '/v1/registry/nodes', { token: IMPORT })).toEqual(
			refused(401, 'invalid_token'),
		);
		expect((await call('GET', '/v1/registry/nodes', { token: READ })).status).toBe(200);
	});

	it('refuses an import with the read token', async () => {
		const call = control();
		const answer = await call('PUT', '/v1/registry/import', { token: READ, body: body() });
		expect(answer).toEqual(refused(401, 'invalid_token'));
	});

	it('closes a route whose token is not set, whatever the bearer', async () => {
		const call = control(new Sqlite(), {
			CONTROL_READ_TOKEN: '',
			CONTROL_WRITE_TOKEN: undefined,
			CONTROL_IMPORT_TOKEN: undefined,
		});
		expect(await call('GET', '/v1/registry/apps', { token: '' })).toEqual(
			refused(503, 'service_unavailable'),
		);
		const answer = await call('PUT', '/v1/registry/import', { token: IMPORT, body: body() });
		expect(answer).toEqual(refused(503, 'service_unavailable'));
		expect((await call('GET', '/v1/health')).status).toBe(200);
	});
});

describe('reads', () => {
	it('lists a kind with the newest change, a node and an account with no name', async () => {
		const { call } = await imported();
		const nodes = await call('GET', '/v1/registry/nodes', { token: READ });
		expect(nodes.body.data).toEqual({
			items: [
				{ kind: 'node', id: TYO, name: null, revision: 11, spec: body().nodes[1]?.spec },
				{ kind: 'node', id: SIN, name: null, revision: 12, spec: body().nodes[0]?.spec },
			],
			revision: 41,
		});
		const apps = await call('GET', '/v1/registry/apps', { token: READ });
		expect(apps.body.data).toMatchObject({
			items: [{ name: 'cdn' }, { name: 'geo' }, { name: 'web' }],
		});
		const accounts = await call('GET', '/v1/registry/accounts', { token: READ });
		expect(accounts.body.data).toMatchObject({ items: [{ id: ORACLE, name: null }], revision: 41 });
	});

	it('gets one by its id, and an app by its name', async () => {
		const { call } = await imported();
		const web = { kind: 'app', id: WEB, name: 'web', revision: 30, spec: { placements: [TYO] } };
		expect((await call('GET', `/v1/registry/apps/${WEB}`, { token: READ })).body.data).toEqual(web);
		expect((await call('GET', '/v1/registry/apps/web', { token: READ })).body.data).toEqual(web);
		expect(
			(await call('GET', `/v1/registry/nodes/${TYO}`, { token: READ })).body.data,
		).toMatchObject({
			id: TYO,
			name: null,
		});
	});

	it('answers what is not held as the old registry does', async () => {
		const { call } = await imported();
		const get = (path: string) => call('GET', path, { token: READ });
		expect(await get('/v1/registry/nodes/tyo')).toEqual(refused(404, 'no_such_node'));
		expect(await get('/v1/registry/nodes/ffffffffffffffff')).toEqual(refused(404, 'no_such_node'));
		expect(await get('/v1/registry/apps/nothing')).toEqual(refused(404, 'no_such_resource'));
		expect(await get(`/v1/registry/accounts/${WEB}`)).toEqual(refused(404, 'no_such_resource'));
		expect(await get('/v1/registry/accounts/oracle')).toEqual(refused(404, 'no_such_resource'));
	});

	it('filters apps by the node id they are placed on', async () => {
		const { call } = await imported();
		const on = async (node: string) => {
			const answer = await call('GET', `/v1/registry/apps?node=${node}`, { token: READ });
			return (answer.body.data as { items: { name: string }[] }).items.map((placed) => placed.name);
		};
		expect(await on(TYO)).toEqual(['geo', 'web']);
		expect(await on(SIN)).toEqual(['geo']);
		expect(await on('ffffffffffffffff')).toEqual([]);
		const named = await call('GET', `/v1/registry/apps?node=${SIN}&name=web`, { token: READ });
		expect(named.body.data).toEqual({ items: [], revision: 41 });
	});

	it('refuses a filter a kind does not have', async () => {
		const { call } = await imported();
		const get = (path: string) => call('GET', path, { token: READ });
		expect(await get('/v1/registry/nodes?name=tyo')).toEqual(refused(400, 'invalid_name'));
		expect(await get('/v1/registry/accounts?name=oracle')).toEqual(refused(400, 'invalid_name'));
		expect(await get(`/v1/registry/nodes?node=${TYO}`)).toEqual(
			refused(400, 'forbidden_parameter'),
		);
		expect(await get('/v1/registry/apps?node=tyo')).toEqual(refused(400, 'invalid_resource'));
		const web = await get('/v1/registry/apps?name=web');
		expect(web.body.data).toMatchObject({ items: [{ id: WEB }], revision: 41 });
	});
});

/** `body()` past what control holds, changed by `change`. */
function changed(change: (sent: ReturnType<typeof body>) => void) {
	const sent = body();
	sent.revision = 50;
	change(sent);
	return sent;
}

describe('import', () => {
	it('leaves the same rows when run again', async () => {
		const { db, call } = await imported();
		const held = db.db.prepare('SELECT kind, id, name, revision, spec FROM resources ORDER BY id');
		const before = held.all();
		const again = await call('PUT', '/v1/registry/import', { token: IMPORT, body: body() });
		expect(again.body.data).toEqual({ revision: 41 });
		expect(held.all()).toEqual(before);
		expect(db.db.prepare('SELECT id, kind FROM changes').all()).toEqual([
			{ id: 41, kind: 'import' },
		]);
	});

	it('replaces every row on a later run, what is gone included', async () => {
		const { call } = await imported();
		const next = body();
		next.revision = 45;
		next.apps = [{ id: GEO, name: 'geo', revision: 44, spec: { placements: [SIN] } }];
		expect(
			(await call('PUT', '/v1/registry/import', { token: IMPORT, body: next })).body.data,
		).toEqual({
			revision: 45,
		});
		const apps = await call('GET', '/v1/registry/apps', { token: READ });
		expect(apps.body.data).toEqual({
			items: [{ kind: 'app', id: GEO, name: 'geo', revision: 44, spec: { placements: [SIN] } }],
			revision: 45,
		});
	});

	it('numbers the next change after the revision imported', async () => {
		const { db } = await imported();
		const sqlite = db.db;
		sqlite.prepare("INSERT INTO changes (at, by, kind) VALUES ('now', 'test', 'app')").run();
		expect(sqlite.prepare('SELECT max(id) AS id FROM changes').get()).toEqual({ id: 42 });
	});

	it('refuses an import older than what control holds, and writes nothing', async () => {
		const { call } = await imported();
		const older = { ...body(), revision: 40, apps: [] };
		const answer = await call('PUT', '/v1/registry/import', { token: IMPORT, body: older });
		expect(answer).toEqual(refused(409, 'invalid_revision'));
		const apps = await call('GET', '/v1/registry/apps', { token: READ });
		expect((apps.body.data as { items: unknown[] }).items).toHaveLength(3);
	});

	it('stores a spec with its keys sorted, at every depth', async () => {
		const db = new Sqlite();
		const call = control(db);
		const spec = { z: 1, a: { y: [{ b: 1, a: 2 }], x: null } };
		const unsorted = { ...body(), accounts: [{ id: ORACLE, name: null, revision: 5, spec }] };
		await call('PUT', '/v1/registry/import', { token: IMPORT, body: unsorted });
		const row = db.db.prepare('SELECT spec FROM resources WHERE id = ?').get(ORACLE);
		expect(row).toEqual({ spec: '{"a":{"x":null,"y":[{"a":2,"b":1}]},"z":1}' });
		const answer = await call('GET', `/v1/registry/accounts/${ORACLE}`, { token: READ });
		expect(JSON.stringify((answer.body.data as { spec: unknown }).spec)).toBe(row?.spec);
	});

	it('refuses a body whose shape or parts do not hold, and writes nothing', async () => {
		const { call } = await imported();
		const put = (sent: unknown) =>
			call('PUT', '/v1/registry/import', { token: IMPORT, body: sent });
		const cases: unknown[] = [
			'not an import',
			{ ...body(), extra: true },
			changed((sent) => Object.assign(sent.nodes[0] ?? {}, { name: 'sin' })),
			changed((sent) => Object.assign(sent.apps[0] ?? {}, { name: 'Web' })),
			changed((sent) => Object.assign(sent.apps[0] ?? {}, { name: TYO })),
			changed((sent) => Object.assign(sent.apps[0] ?? {}, { spec: { placements: ['tyo'] } })),
			changed((sent) => Object.assign(sent.apps[0] ?? {}, { spec: { placements: [ORACLE] } })),
			changed((sent) => Object.assign(sent.apps[0] ?? {}, { spec: { placements: [TYO, TYO] } })),
			changed((sent) => Object.assign(sent.apps[0] ?? {}, { spec: {} })),
			changed((sent) => Object.assign(sent.apps[0] ?? {}, { revision: 51 })),
			changed((sent) => Object.assign(sent.apps[0] ?? {}, { id: GEO })),
			changed((sent) => Object.assign(sent.apps[0] ?? {}, { name: 'geo' })),
			changed((sent) => Object.assign(sent.accounts[0] ?? {}, { spec: [] })),
			changed((sent) => Object.assign(sent, { revision: 1.5 })),
		];
		const answers = await Promise.all(cases.map(put));
		for (const [index, answer] of answers.entries()) {
			expect(answer, JSON.stringify(cases[index])).toEqual(refused(400, 'invalid_body'));
		}
		const apps = await call('GET', '/v1/registry/apps', { token: READ });
		expect(apps.body.data).toMatchObject({ revision: 41 });
	});

	it('writes nothing when one statement of the batch fails', async () => {
		const { db, call } = await imported();
		const broken = [{ kind: 'node' as const, id: TYO, name: 'tyo', revision: 1, spec: '{}' }];
		await expect(replaced(db, broken, [], 50, 'now')).rejects.toThrow();
		const nodes = await call('GET', '/v1/registry/nodes', { token: READ });
		expect(nodes.body.data).toMatchObject({ items: [{ id: TYO }, { id: SIN }], revision: 41 });
	});
});

describe('envelope', () => {
	it('answers a path, a kind and a method nothing takes in the envelope', async () => {
		const call = control();
		expect(await call('GET', '/')).toEqual(refused(404, 'no_such_route'));
		expect(await call('GET', '/v2/nodes')).toEqual(refused(404, 'no_such_route'));
		expect(await call('GET', '/v1/nodes', { token: READ })).toEqual(refused(404, 'no_such_scope'));
		expect(await call('PUT', '/v1/import', { token: IMPORT })).toEqual(
			refused(404, 'no_such_scope'),
		);
		expect(await call('GET', `/v1/nodes/${TYO}`, { token: READ })).toEqual(
			refused(404, 'no_such_scope'),
		);
		expect(await call('GET', '/v1/registry', { token: READ })).toEqual(
			refused(404, 'no_such_route'),
		);
		expect(await call('GET', `/v1/registry/nodes/${TYO}/more`, { token: READ })).toEqual(
			refused(404, 'no_such_route'),
		);
		expect(await call('GET', '/v1/registry/toString', { token: READ })).toEqual(
			refused(404, 'no_such_kind'),
		);
		expect(await call('DELETE', '/v1/registry/nodes', { token: READ })).toEqual(
			refused(405, 'invalid_method'),
		);
		expect(await call('DELETE', '/v1/registry/things', { token: READ })).toEqual(
			refused(404, 'no_such_kind'),
		);
		expect(await call('GET', '/v1/registry/import', { token: IMPORT })).toEqual(
			refused(405, 'invalid_method'),
		);
		const unreadable = await app.fetch(
			new Request(new URL('/v1/registry/import', ORIGIN), {
				method: 'PUT',
				headers: { Authorization: `Bearer ${IMPORT}` },
				body: '{',
			}),
			{ DB: new Sqlite(), CONTROL_IMPORT_TOKEN: IMPORT },
		);
		expect(unreadable.status).toBe(400);
	});
});

describe('canonical', () => {
	it('sorts keys and keeps the order of a list', () => {
		expect(canonical({ b: [3, 1, { d: 1, c: 2 }], a: 'x' })).toBe(
			'{"a":"x","b":[3,1,{"c":2,"d":1}]}',
		);
	});
});
