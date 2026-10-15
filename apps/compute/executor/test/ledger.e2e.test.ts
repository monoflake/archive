import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest';
import { Driver } from '../src/driver.ts';
import { ledgerAt } from '../src/ledger.ts';
import { Node, WORKFLOWS } from './e2e/node.ts';
import { LEDGER_NODE, ready, until, World } from './e2e/world.ts';

/**
 * The executor against the real ledger and a real Postgres 18, mirroring the ledger's own
 * apps/data/ledger/src/store/runs/tests.rs from the node's side. A step placed on a node is not
 * built yet, and neither are its cases.
 */
const world = new World();
const drivers: Driver[] = [];

function drive(node: string, options: Partial<ConstructorParameters<typeof Driver>[0]> = {}) {
	const at = new Driver({
		node,
		capacity: 4,
		workflows: Object.keys(WORKFLOWS),
		ledger: ledgerAt(world.ledger),
		start: new Node(world, node).start,
		pollMs: [100, 200],
		log: () => {},
		...options,
	});
	drivers.push(at);
	at.start();
	return at;
}

describe.runIf(ready())('the executor against the ledger', () => {
	beforeAll(() => world.start(), 30_000);
	afterAll(() => world.stop());

	// Each test meets a ledger with no executor alive and no run left waiting from the last.
	beforeEach(async () => {
		await world.sql`UPDATE executors SET seen_at = 0`;
		await world.sql`DELETE FROM leases`;
		await world.sql`UPDATE runs SET status = 'failed' WHERE status NOT IN ('done', 'failed')`;
	});
	afterEach(async () => {
		await Promise.all(drivers.splice(0).map((at) => at.stop(0)));
	});

	it('runs a run as a task, a done step once however often it is replayed', async () => {
		await world.open('issue-1', 'issue');
		const node = new Node(world, '9d399312d1c3ddc4');
		const first = await node.turn('issue-1');
		expect(first.ended).toEqual({ ended: 'halted', why: 'turn' });
		const sleeping = await world.view('issue-1');
		expect(sleeping.run.status).toBe('sleeping');
		expect(sleeping.lease, 'a sleeping run holds no node').toBeNull();
		const wake = Date.parse(
			sleeping.steps.find((step) => step.name === 'propagate')?.wake_at ?? '',
		);
		expect(wake - Date.now()).toBeGreaterThan(3_500_000);

		await world.due();
		const second = await node.turn('issue-1', first.turn.fence);
		expect(second.ended).toEqual({ ended: 'done', output: { kept: 'o-1' } });
		expect([world.witness.count('/order'), world.witness.count('/keep')]).toEqual([1, 1]);
		const done = await world.view('issue-1');
		expect(done.run).toMatchObject({ status: 'done', fence: 2, output: { kept: 'o-1' } });
		const task = await world.call('GET', '/tasks/e2e/issue-1');
		const stages = (task.data.events as { stage: string }[]).map((event) => event.stage);
		for (const stage of [
			'queued',
			'running',
			'lease',
			'step.order',
			'step.propagate',
			'step.keep',
			'done',
		]) {
			expect(stages, stage).toContain(stage);
		}
	});

	it('loses nothing when a node is lost mid-step, and refuses its late report', async () => {
		await world.open('lost-1', 'hold');
		world.witness.hold('/a/hold/lost-1');
		const lost = new Node(world, '9d399312d1c3ddc4', undefined, `${world.witness.base}/a`);
		const survivor = new Node(world, 'd62c82b23a858876', undefined, `${world.witness.base}/b`);
		const turn = await lost.granted('lost-1');
		const running = lost.start(turn);
		await until('the lost node begins the step', () => world.witness.count('/a/hold/lost-1') === 1);
		await lost.poll(0);
		await world.lapse();
		const taken = await survivor.turn('lost-1', turn.fence);
		expect(taken.ended).toEqual({ ended: 'done', output: 'held' });
		world.witness.release('/a/hold/lost-1');
		expect(await running.ended).toEqual({ ended: 'halted', why: 'stale' });
		const view = await world.view('lost-1');
		expect(view.run).toMatchObject({ status: 'done', fence: 2, output: 'held' });
		expect(view.steps[0]).toMatchObject({ attempt: 1, starts: 2, status: 'done' });
	});

	it('retries a failed step after its backoff and stops at its limit', async () => {
		await world.open('flaky-1', 'flaky');
		const node = new Node(world, '9d399312d1c3ddc4');
		const first = await node.turn('flaky-1');
		expect(first.ended).toEqual({ ended: 'halted', why: 'turn' });
		expect(await world.status('flaky-1')).toBe('sleeping');
		await world.due();
		const second = await node.turn('flaky-1', first.turn.fence);
		expect(second.ended).toEqual({ ended: 'halted', why: 'turn' });
		expect(await world.status('flaky-1')).toBe('sleeping');
		await world.due();
		const last = await node.turn('flaky-1', second.turn.fence);
		expect(last.ended).toEqual({ ended: 'failed', error: 'the store said no' });
		expect((await world.view('flaky-1')).steps[0]).toMatchObject({ attempt: 3, status: 'failed' });
		expect(world.witness.count('/flaky')).toBe(3);
	});

	it('never retries a NonRetryableError', async () => {
		await world.open('fatal-1', 'fatal');
		const { ended } = await new Node(world, '9d399312d1c3ddc4').turn('fatal-1');
		expect(ended).toEqual({ ended: 'failed', error: 'never' });
		expect((await world.view('fatal-1')).steps[0]).toMatchObject({ attempt: 1, status: 'failed' });
		expect(world.witness.count('/doomed')).toBe(1);
	});

	it('fails a step begun more than twenty times by holders that were lost', async () => {
		await world.open('starts-1', 'hold');
		world.witness.hold('/hold/starts-1');
		const node = new Node(world, '9d399312d1c3ddc4');
		const turn = await node.granted('starts-1');
		const running = node.start(turn);
		await until('the step begins', () => world.witness.count('/hold/starts-1') === 1);
		running.terminate();
		await running.ended;
		await world.sql`UPDATE steps SET starts = 20 WHERE run = 'starts-1'`;
		await world.lapse();
		const { ended } = await node.turn('starts-1', turn.fence);
		expect(ended).toMatchObject({ ended: 'failed' });
		expect((await world.view('starts-1')).steps[0]?.status).toBe('failed');
		world.witness.release('/hold/starts-1');
	});

	it('counts a step found past its deadline as a failed attempt', async () => {
		await world.open('deadline-1', 'hold');
		world.witness.hold('/hold/deadline-1');
		const node = new Node(world, '9d399312d1c3ddc4');
		const turn = await node.granted('deadline-1');
		const running = node.start(turn);
		await until('the step begins', () => world.witness.count('/hold/deadline-1') === 1);
		running.terminate();
		await running.ended;
		await world.sql`UPDATE steps SET deadline = 1 WHERE run = 'deadline-1'`;
		await world.lapse();
		const { ended } = await node.turn('deadline-1', turn.fence);
		expect(ended).toMatchObject({ ended: 'failed' });
		expect((await world.view('deadline-1')).steps[0]?.status).toBe('failed');
		world.witness.release('/hold/deadline-1');
	});

	it('fails a step whose result is past 1 MiB', async () => {
		await world.open('huge-1', 'huge');
		const { ended } = await new Node(world, '9d399312d1c3ddc4').turn('huge-1');
		expect(ended).toMatchObject({ ended: 'failed' });
		expect((await world.view('huge-1')).steps[0]?.status).toBe('failed');
	});

	it('fails a run whose code disagrees with the record', async () => {
		await world.open('diverged-1', 'before');
		const node = new Node(world, '9d399312d1c3ddc4');
		const first = await node.turn('diverged-1');
		expect(first.ended).toEqual({ ended: 'halted', why: 'turn' });
		await world.sql`UPDATE runs SET workflow = 'after' WHERE id = 'diverged-1'`;
		await world.due();
		const second = await node.turn('diverged-1', first.turn.fence);
		expect(second.ended).toMatchObject({
			ended: 'failed',
			error: expect.stringMatching(/^replay_diverged: /),
		});
		expect(await world.status('diverged-1')).toBe('failed');
	});

	it('waits for an event holding nothing, takes it once, and says when none came', async () => {
		await world.open('approve-1', 'approve');
		const node = new Node(world, '9d399312d1c3ddc4');
		const first = await node.turn('approve-1');
		expect(first.ended).toEqual({ ended: 'halted', why: 'turn' });
		expect((await world.view('approve-1')).lease).toBeNull();
		const event = { key: 'k-1', type: 'approval', payload: { by: 'canmi' } };
		const sent = await world.call('POST', '/v1/runs/e2e/approve-1/events', event);
		expect(sent.data).toEqual({ new: true });
		expect((await world.call('POST', '/v1/runs/e2e/approve-1/events', event)).data).toEqual({
			new: false,
		});
		const second = await node.turn('approve-1', first.turn.fence);
		expect(second.ended).toEqual({ ended: 'done', output: { by: 'canmi', type: 'approval' } });

		await world.open('approve-2', 'approve');
		const waiting = await node.turn('approve-2');
		await world.due();
		const unanswered = await node.turn('approve-2', waiting.turn.fence);
		expect(unanswered.ended).toMatchObject({
			ended: 'done',
			output: { unanswered: expect.any(String) },
		});
	});

	it('grants a run only to a node that names its workflow', async () => {
		await world.open('named-1', 'fatal');
		const elsewhere = new Node(world, '9d399312d1c3ddc4', ['issue']);
		await elsewhere.poll();
		await new Promise((resolve) => setTimeout(resolve, 2_500));
		expect(await elsewhere.poll()).toEqual([]);
		expect(await world.status('named-1')).toBe('ready');
		const { ended } = await new Node(world, 'd62c82b23a858876', ['fatal']).turn('named-1');
		expect(ended).toEqual({ ended: 'failed', error: 'never' });
	});

	it('takes a pushed turn at once, and runs it once though polls answer it too', async () => {
		let rung: Driver | undefined;
		const server = createServer((request, response) => {
			void rung?.ring();
			response.statusCode = request.url === '/turns' ? 202 : 404;
			response.end('{}');
		});
		await new Promise<void>((resolve) => server.listen(0, resolve));
		const pushUrl = `http://localhost:${(server.address() as AddressInfo).port}/turns`;
		// Polled once at start and not again within the test: only the doorbell brings the turn.
		rung = drive(LEDGER_NODE, { pollMs: [60_000, 60_000], pushUrl });
		await until('the first poll', () => rung?.polledAt !== undefined);
		await world.open('pushed-1', 'hold');
		await until(
			'the pushed run ending',
			async () => (await world.status('pushed-1')) === 'done',
			5_000,
		);
		expect(world.witness.count('/hold/pushed-1')).toBe(1);
		server.close();
	});

	it('holds no more leases than its capacity, each run once', async () => {
		world.witness.hold('/hold/cap-');
		const at = drive('9d399312d1c3ddc4', { capacity: 1 });
		await world.open('cap-1', 'hold');
		await world.open('cap-2', 'hold');
		await until(
			'one run leased',
			async () =>
				(await world.sql`SELECT 1 FROM leases WHERE holder = '9d399312d1c3ddc4'`).length === 1,
		);
		await new Promise((resolve) => setTimeout(resolve, 2_500));
		const held = await world.sql`SELECT run FROM leases WHERE holder = '9d399312d1c3ddc4'`;
		expect(held).toHaveLength(1);
		world.witness.release('/hold/cap-');
		await until(
			'both runs done',
			async () =>
				(await world.status('cap-1')) === 'done' && (await world.status('cap-2')) === 'done',
		);
		expect([world.witness.count('/hold/cap-1'), world.witness.count('/hold/cap-2')]).toEqual([
			1, 1,
		]);
		expect(at.running).toBe(0);
	});

	it('renews a lease while its turn runs, and stops the turn once a renewal is refused', async () => {
		const lines: string[] = [];
		world.witness.hold('/hold/long-1');
		drive('d62c82b23a858876', { renewMs: 100, log: (line) => lines.push(line) });
		await world.open('long-1', 'hold');
		await until('the step begins', () => world.witness.count('/hold/long-1') === 1);
		await world.sql`UPDATE leases SET expires_at = expires_at - 25000000000 WHERE run = 'long-1'`;
		const [before] = await world.sql`SELECT expires_at FROM leases WHERE run = 'long-1'`;
		const shortened = BigInt(before?.expires_at ?? 0);
		await until('a renewal', async () => {
			const [lease] = await world.sql`SELECT expires_at FROM leases WHERE run = 'long-1'`;
			return BigInt(lease?.expires_at ?? 0) > shortened + 20_000_000_000n;
		});
		// The lease another node's, as a grant to it would make it: only the renewal can say so,
		// since this node's polls no longer answer the turn.
		await world.sql`UPDATE leases SET holder = '9d399312d1c3ddc4' WHERE run = 'long-1'`;
		await until('the turn stopped', () =>
			lines.some((line) => line.includes('its renewal was refused')),
		);
		world.witness.release('/hold/long-1');
	});
});
