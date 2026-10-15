import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { DEPLOY_SOURCES } from '@monoflake/sdk';
import { describe, expect, it, vi } from 'vitest';
import { WORKFLOW } from './github.ts';
import { bindingOf, DEPLOYER, type Node, RECEIVERS } from './hook.ts';
import { app, type Env } from './index.ts';
import type { Database } from './store.ts';
import { imported, ORIGIN, refused, SIN, Sqlite, TYO, WRITE } from './testing.ts';

const SECRET = 'test-secret';
/** infra's nodes.toml, beside this repository. */
const NODES = join(import.meta.dirname, '../../../../../infra/nodes/nodes.toml');
const WRANGLER = join(import.meta.dirname, '../wrangler.jsonc');
/** A node control holds no record of, bound all the same. */
const BRU = '00000000000000c3';

async function sign(body: string): Promise<string> {
	const encoder = new TextEncoder();
	const key = await crypto.subtle.importKey(
		'raw',
		encoder.encode(SECRET),
		{ name: 'HMAC', hash: 'SHA-256' },
		false,
		['sign'],
	);
	const bytes = new Uint8Array(await crypto.subtle.sign('HMAC', key, encoder.encode(body)));
	return `sha256=${Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('')}`;
}

interface Sent {
	node: string;
	url: string;
	body: string;
}

/** A VPC binding that records what it was sent; `answer` is how it answers each receiver. */
function bound(node: string, sent: Sent[], answer: (url: string) => number): Node {
	return {
		fetch: async (url, init) => {
			sent.push({ node, url, body: String(init.body) });
			return new Response(null, { status: answer(url) });
		},
	};
}

/** control over `db`, a binding per node in `statuses` answering with its status. */
function nodes(db: Database, statuses: Record<string, number>) {
	const sent: Sent[] = [];
	const bindings = Object.fromEntries(
		Object.entries(statuses).map(([node, status]) => [
			bindingOf(node),
			bound(node, sent, () => status),
		]),
	);
	return { sent, env: { DB: db, WEBHOOK_SECRET: SECRET, ...bindings } satisfies Env };
}

/** control holding the deployer, placed on `placements`. */
async function placed(placements: string[]) {
	const { db, call } = await imported();
	const spec = { placements };
	const made = await call('PUT', '/v1/registry/apps/deployer', {
		token: WRITE,
		body: { by: 'test', revision: 0, spec },
	});
	expect(made.status).toBe(202);
	return db;
}

const DELIVERY = JSON.stringify({
	action: 'completed',
	repository: { full_name: DEPLOY_SOURCES[0] },
	workflow_run: {
		id: 7,
		path: WORKFLOW,
		head_branch: 'main',
		event: 'push',
		status: 'completed',
		conclusion: 'success',
	},
});

async function deliver(
	env: Env,
	body = DELIVERY,
	event = 'workflow_run',
	signature?: string | null,
) {
	const headers = new Headers({ 'x-github-event': event });
	if (signature !== null) headers.set('x-hub-signature-256', signature ?? (await sign(body)));
	const request = new Request(new URL('/v1/hook/github', ORIGIN), {
		method: 'POST',
		headers,
		body,
	});
	return app.fetch(request, env);
}

describe('the hook scope', () => {
	it('passes a deployable run to host and keeper on every node, and the deployer on its own', async () => {
		const { sent, env } = nodes(await placed([TYO]), { [TYO]: 204, [SIN]: 204, [BRU]: 204 });
		const answer = await deliver(env);
		expect(answer.status).toBe(202);
		for (const node of [SIN, BRU]) {
			expect(sent.filter((each) => each.node === node).map((each) => each.url)).toEqual(RECEIVERS);
		}
		const tyo = sent.filter((each) => each.node === TYO).map((each) => each.url);
		expect(tyo).toEqual([...RECEIVERS, DEPLOYER]);
		expect(sent.every((each) => JSON.parse(each.body).run === 7)).toBe(true);
		const named = sent.map((each) => JSON.parse(each.body).repository);
		expect(named.every((repository) => repository === DEPLOY_SOURCES[0])).toBe(true);
		expect(DEPLOYER).toMatch(/^http:\/\/deployer\.[^/]+\/notice$/);
	});

	it("takes the deployer's nodes from control, wherever it is placed now", async () => {
		const { sent, env } = nodes(await placed([SIN, TYO]), { [TYO]: 204, [SIN]: 204 });
		expect((await deliver(env)).status).toBe(202);
		const told = sent.filter((each) => each.url === DEPLOYER).map((each) => each.node);
		expect(told).toEqual([SIN, TYO]);
	});

	it('tells no deployer while control holds none', async () => {
		const { sent, env } = nodes((await imported()).db, { [TYO]: 204 });
		expect((await deliver(env)).status).toBe(202);
		expect(sent.map((each) => each.url)).toEqual(RECEIVERS);
	});

	it('refuses a delivery whose signature is wrong or missing, telling nobody', async () => {
		const { sent, env } = nodes(new Sqlite(), { [TYO]: 204 });
		const answers = await Promise.all([
			deliver(env, DELIVERY, 'workflow_run', 'sha256=00'),
			deliver(env, DELIVERY, 'workflow_run', null),
			deliver({ ...env, WEBHOOK_SECRET: undefined }),
		]);
		const bodies = await Promise.all(answers.map((answer) => answer.json()));
		expect(answers.map((answer) => answer.status)).toEqual([401, 401, 401]);
		for (const each of bodies) expect(each).toMatchObject({ code: 'invalid_signature' });
		expect(sent).toHaveLength(0);
	});

	it('answers a ping and a run not to deploy without passing either on', async () => {
		const { sent, env } = nodes(new Sqlite(), { [TYO]: 204 });
		expect((await deliver(env, '{}', 'ping')).status).toBe(204);
		const failed = JSON.parse(DELIVERY);
		failed.workflow_run.conclusion = 'failure';
		expect((await deliver(env, JSON.stringify(failed))).status).toBe(204);
		expect(sent).toHaveLength(0);
	});

	it('does not succeed when one node fails, and still tells the others', async () => {
		const { sent, env } = nodes(await placed([TYO]), { [TYO]: 204, [SIN]: 502, [BRU]: 204 });
		const log = vi.spyOn(console, 'error').mockImplementation(() => {});
		const answer = await deliver(env);
		expect(answer.status).toBe(502);
		expect(await answer.json()).toMatchObject({ status: 'error', code: 'upstream_unavailable' });
		expect(sent).toHaveLength(RECEIVERS.length * 3 + 1);
		// One line per receiver that missed, naming the node.
		expect(log).toHaveBeenCalledTimes(RECEIVERS.length);
		expect(log.mock.calls.every(([line]) => String(line).includes(SIN))).toBe(true);
		log.mockRestore();
	});

	it('fails the delivery when the deployer does not take it, or is placed on no bound node', async () => {
		const sent: Sent[] = [];
		const tyo = bound(TYO, sent, (url) => (url === DEPLOYER ? 404 : 204));
		const log = vi.spyOn(console, 'error').mockImplementation(() => {});
		const env = {
			DB: await placed([TYO]),
			WEBHOOK_SECRET: SECRET,
			...Object.fromEntries([[bindingOf(TYO), tyo]]),
		} satisfies Env;
		expect((await deliver(env)).status).toBe(502);
		expect(sent.map((each) => each.url)).toEqual([...RECEIVERS, DEPLOYER]);
		const elsewhere = { ...env, DB: await placed([SIN]) };
		const unbound = await deliver(elsewhere);
		expect(unbound.status).toBe(502);
		expect(((await unbound.json()) as { message: string }).message).toContain(SIN);
		log.mockRestore();
	});

	it('does not stop at a node whose binding throws', async () => {
		const { sent, env } = nodes(new Sqlite(), { [TYO]: 204 });
		const down: Node = {
			fetch: async () => {
				throw new Error('tunnel down');
			},
		};
		const log = vi.spyOn(console, 'error').mockImplementation(() => {});
		const answer = await deliver({ ...env, [bindingOf(SIN)]: down });
		expect(answer.status).toBe(502);
		expect(sent).toHaveLength(RECEIVERS.length);
		log.mockRestore();
	});

	it('answers 502 with no node bound, and 500 telling nobody when the store is down', async () => {
		expect((await deliver({ DB: new Sqlite(), WEBHOOK_SECRET: SECRET })).status).toBe(502);
		const { sent, env } = nodes(new Sqlite(), { [TYO]: 204 });
		const broken: Database = {
			prepare: () => {
				throw new Error('D1 is down');
			},
			batch: () => Promise.reject(new Error('D1 is down')),
		};
		const log = vi.spyOn(console, 'error').mockImplementation(() => {});
		const answer = await deliver({ ...env, DB: broken });
		expect({ status: answer.status, body: await answer.json() }).toEqual(
			refused(500, 'store_unavailable'),
		);
		expect(sent).toHaveLength(0);
		log.mockRestore();
	});

	it('takes nothing but a POST, and its scope is known', async () => {
		const { env } = nodes(new Sqlite(), { [TYO]: 204 });
		const got = await app.fetch(new Request(new URL('/v1/hook/github', ORIGIN)), env);
		expect(got.status).toBe(405);
		const other = await app.fetch(new Request(new URL('/v1/hook/gitlab', ORIGIN)), env);
		expect(await other.json()).toMatchObject({ code: 'no_such_route' });
	});
});

describe("control's VPC bindings", () => {
	it.skipIf(!existsSync(NODES))('bind every node nodes.toml holds, by its id, in its order', () => {
		const ids = [...readFileSync(NODES, 'utf8').matchAll(/^id = "([0-9a-f]{16})"$/gm)];
		const bindings = [...readFileSync(WRANGLER, 'utf8').matchAll(/"binding": "(NODE_\w+)"/g)];
		expect(ids.length).toBeGreaterThan(0);
		expect(bindings.map((match) => match[1])).toEqual(ids.map((match) => bindingOf(match[1]!)));
	});

	it("stay under the free plan's 50 subrequests a delivery, the deployer on every node", () => {
		const bindings = [...readFileSync(WRANGLER, 'utf8').matchAll(/"binding": "(NODE_\w+)"/g)];
		// Two receivers a node, a deployer at most on each, and the one read of its placements.
		expect(bindings.length * (RECEIVERS.length + 1) + 1).toBeLessThan(50);
	});
});
