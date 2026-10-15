import { afterEach, describe, expect, it, vi } from 'vitest';
import { RECEIVERS } from './hook.ts';
import type { Env } from './index.ts';
import { bindingOf, type Node } from './nodes.ts';
import { HOST } from './notice.ts';
import {
	context,
	control,
	imported,
	READ,
	refused,
	SIN,
	shown,
	Sqlite,
	TYO,
	WRITE,
} from './testing.ts';

afterEach(() => {
	vi.restoreAllMocks();
});

/** A node control holds no record of, bound all the same. */
const BRU = '00000000000000c3';

interface Sent {
	node: string;
	url: string;
	body: unknown;
}

/** A VPC binding per node of `answers`, answering as its function does, recording each send. */
function bindings(answers: Record<string, () => Promise<Response>>) {
	const sent: Sent[] = [];
	const env = Object.fromEntries(
		Object.entries(answers).map(([node, answer]): [string, Node] => [
			bindingOf(node),
			{
				fetch: async (url, init) => {
					sent.push({ node, url, body: JSON.parse(String(init.body)) });
					return answer();
				},
			},
		]),
	) as Partial<Env>;
	return { sent, env };
}

const taking = () => Promise.resolve(new Response(null, { status: 204 }));

/** control holding the import, over `env`'s bindings, its `waitUntil` kept. */
async function bound(env: Partial<Env>) {
	const { db } = await imported();
	const kept = context();
	const call = control(db, env, kept);
	const write = (method: string, path: string, revision: number, spec: unknown) =>
		call(method, path, { token: WRITE, body: { by: 'test', revision, spec } });
	return { call, write, waited: kept.waited };
}

describe('a registry write that lands', () => {
	it('tells host on every bound node the new revision, after the answer, through waitUntil', async () => {
		const { sent, env } = bindings({ [TYO]: taking, [SIN]: taking, [BRU]: taking });
		const { write, waited } = await bound(env);
		const answer = await write('PATCH', '/v1/registry/apps/web', 30, { display_name: 'Web' });
		expect(answer.status).toBe(202);
		expect(waited).toHaveLength(1);
		await Promise.all(waited);
		expect(sent.map(({ node }) => node).toSorted()).toEqual([BRU, SIN, TYO].toSorted());
		for (const each of sent) {
			expect(each).toMatchObject({ url: HOST, body: { changed: 'registry', revision: 42 } });
		}
		expect(HOST).toBe(RECEIVERS[0]);
		expect(HOST).toMatch(/^http:\/\/infra\.[^/]+\/notice$/);
	});

	it('tells each kind of write: a PUT, a PATCH and a POST', async () => {
		const { sent, env } = bindings({ [TYO]: taking });
		const { write, waited } = await bound(env);
		expect((await write('PUT', '/v1/registry/apps/fresh', 0, { placements: [] })).status).toBe(202);
		const node = shown('100.64.0.9', 'Raleigh', 'US');
		expect((await write('POST', '/v1/registry/nodes', 0, node)).status).toBe(202);
		expect((await write('PATCH', `/v1/registry/nodes/${TYO}`, 11, { city: 'Osaka' })).status).toBe(
			202,
		);
		await Promise.all(waited);
		expect(sent.map(({ body }) => body)).toEqual(
			[42, 43, 44].map((revision) => ({
				changed: 'registry',
				revision,
			})),
		);
	});

	it('never waits on a node that does not answer', async () => {
		const { sent, env } = bindings({ [TYO]: () => new Promise<Response>(() => {}) });
		const { write, waited } = await bound(env);
		const answer = await write('PATCH', '/v1/registry/apps/web', 30, { display_name: 'Web' });
		expect(answer.status).toBe(202);
		expect(waited).toHaveLength(1);
		expect(sent).toHaveLength(1);
	});

	it('never fails for a node that refuses it or whose binding throws, and logs each', async () => {
		const log = vi.spyOn(console, 'error').mockImplementation(() => {});
		const { sent, env } = bindings({
			[TYO]: taking,
			[SIN]: () => Promise.resolve(new Response(null, { status: 404 })),
			[BRU]: () => Promise.reject(new Error('tunnel down')),
		});
		const { write, waited } = await bound(env);
		const answer = await write('PATCH', '/v1/registry/apps/web', 30, { display_name: 'Web' });
		expect(answer).toMatchObject({ status: 202, body: { data: { revision: 42 } } });
		await Promise.all(waited);
		expect(sent).toHaveLength(3);
		expect(log).toHaveBeenCalledTimes(2);
		const lines = log.mock.calls.map(([line]) => String(line));
		expect(lines.some((line) => line.includes(SIN) && line.includes('404'))).toBe(true);
		expect(lines.some((line) => line.includes(BRU) && line.includes('tunnel down'))).toBe(true);
	});
});

describe('a registry write that does not land', () => {
	it('tells nobody when nothing changed, or the write is refused', async () => {
		const { sent, env } = bindings({ [TYO]: taking });
		const { write, waited } = await bound(env);
		expect(await write('PATCH', '/v1/registry/apps/web', 30, {})).toMatchObject({ status: 200 });
		const same = { placements: [TYO] };
		expect(await write('PUT', '/v1/registry/apps/web', 30, same)).toMatchObject({ status: 200 });
		expect(await write('PATCH', '/v1/registry/apps/web', 29, { a: 1 })).toEqual(
			refused(409, 'invalid_revision'),
		);
		expect(waited).toHaveLength(0);
		expect(sent).toHaveLength(0);
	});
});

describe('the notice scope', () => {
	it('tells every bound node that the secrets changed, and lists each', async () => {
		const { sent, env } = bindings({ [TYO]: taking, [SIN]: taking });
		const { call } = await bound(env);
		const answer = await call('POST', '/v1/notice', {
			token: WRITE,
			body: { changed: 'secrets' },
		});
		expect(answer.status).toBe(202);
		const reached = (answer.body.data as { reached: unknown[] }).reached;
		expect(reached).toHaveLength(2);
		expect(reached).toEqual(
			expect.arrayContaining([
				{ node: TYO, status: 204, ok: true },
				{ node: SIN, status: 204, ok: true },
			]),
		);
		expect(sent.every(({ url }) => url === HOST)).toBe(true);
		expect(sent.map(({ body }) => body)).toEqual([{ changed: 'secrets' }, { changed: 'secrets' }]);
	});

	it('attaches the revision control holds to a registry notice', async () => {
		const { sent, env } = bindings({ [TYO]: taking });
		const { call } = await bound(env);
		const answer = await call('POST', '/v1/notice', {
			token: WRITE,
			body: { changed: 'registry' },
		});
		expect(answer).toEqual({
			status: 202,
			body: { status: 'success', data: { reached: [{ node: TYO, status: 204, ok: true }] } },
		});
		expect(sent.map(({ body }) => body)).toEqual([{ changed: 'registry', revision: 41 }]);
	});

	it('lists a node that missed it, and still succeeds', async () => {
		const log = vi.spyOn(console, 'error').mockImplementation(() => {});
		const { env } = bindings({
			[TYO]: taking,
			[SIN]: () => Promise.resolve(new Response(null, { status: 400 })),
			[BRU]: () => Promise.reject(new Error('tunnel down')),
		});
		const { call } = await bound(env);
		const answer = await call('POST', '/v1/notice', {
			token: WRITE,
			body: { changed: 'secrets' },
		});
		expect(answer.status).toBe(202);
		expect((answer.body.data as { reached: unknown[] }).reached).toEqual(
			expect.arrayContaining([
				{ node: TYO, status: 204, ok: true },
				{ node: SIN, status: 400, ok: false },
				{ node: BRU, status: 'Error: tunnel down', ok: false },
			]),
		);
		expect(log).toHaveBeenCalledTimes(2);
	});

	it('answers with an empty list while no node is bound', async () => {
		const call = control();
		expect(
			await call('POST', '/v1/notice', { token: WRITE, body: { changed: 'secrets' } }),
		).toEqual({ status: 202, body: { status: 'success', data: { reached: [] } } });
	});

	it('takes the write token alone, and is closed without one', async () => {
		const { sent, env } = bindings({ [TYO]: taking });
		const call = control(new Sqlite(), env);
		const body = { changed: 'secrets' };
		expect(await call('POST', '/v1/notice', { body })).toEqual(refused(401, 'invalid_token'));
		expect(await call('POST', '/v1/notice', { token: READ, body })).toEqual(
			refused(401, 'invalid_token'),
		);
		const closed = control(new Sqlite(), { ...env, CONTROL_WRITE_TOKEN: undefined });
		expect(await closed('POST', '/v1/notice', { token: WRITE, body })).toEqual(
			refused(503, 'service_unavailable'),
		);
		expect(sent).toHaveLength(0);
	});

	it('refuses any other body, telling nobody', async () => {
		const { sent, env } = bindings({ [TYO]: taking });
		const call = control(new Sqlite(), env);
		const bodies = [
			undefined,
			'secrets',
			[],
			{},
			{ changed: 'apps' },
			{ changed: 'registry', revision: 5 },
			{ changed: 'secrets', by: 'operator' },
		];
		const answers = await Promise.all(
			bodies.map((body) => call('POST', '/v1/notice', { token: WRITE, body })),
		);
		expect(answers).toEqual(bodies.map(() => refused(400, 'invalid_body')));
		expect(sent).toHaveLength(0);
	});

	it('takes nothing but a POST, and its scope is known', async () => {
		const call = control();
		expect(await call('GET', '/v1/notice', { token: WRITE })).toEqual(
			refused(405, 'invalid_method'),
		);
		expect(await call('POST', '/v1/notice/secrets', { token: WRITE })).toEqual(
			refused(404, 'no_such_route'),
		);
	});
});
