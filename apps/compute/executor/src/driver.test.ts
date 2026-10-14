import type { Ended, LedgerFetch, Turn } from '@monoflake/replay';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Driver, type DriverOptions, RENEW_MS } from './driver.ts';

/** The ledger's two executor routes in memory: the turns a poll answers, and renewals. */
class Ledger {
	turns: Turn[] = [];
	polls: Record<string, unknown>[] = [];
	renewals: Record<string, unknown>[] = [];
	/** What a renewal answers: a later expiry, a refusal, or nothing at all. */
	renewal: 'renewed' | 'stale' | 'down' = 'renewed';

	readonly fetch: LedgerFetch = async (path, init) => {
		const body = JSON.parse(String(init.body)) as Record<string, unknown>;
		if (path.endsWith('/polls')) {
			this.polls.push(body);
			return Response.json({ status: 'success', data: { turns: this.turns } });
		}
		this.renewals.push({ path, ...body });
		if (this.renewal === 'down') throw new TypeError('fetch failed');
		if (this.renewal === 'stale') {
			return Response.json({ status: 'error', code: 'stale_lease', message: '' }, { status: 409 });
		}
		const expires_at = new Date(Date.now() + 30_000).toISOString();
		return Response.json({ status: 'success', data: { expires_at } });
	};
}

/** A turn that runs until the test ends it, and says whether it was terminated. */
class Runs {
	readonly started: Turn[] = [];
	readonly terminated: string[] = [];
	readonly #ends = new Map<string, (ended: Ended) => void>();

	readonly start = (given: Turn) => {
		const key = `${given.run}@${given.fence}`;
		this.started.push(given);
		const ended = new Promise<Ended>((resolve) => this.#ends.set(key, resolve));
		return {
			ended,
			terminate: () => {
				this.terminated.push(key);
				this.#ends.get(key)?.({ ended: 'halted', why: 'unreachable', detail: 'terminated' });
			},
		};
	};

	end(run: string, fence = 1): void {
		this.#ends.get(`${run}@${fence}`)?.({ ended: 'done', output: null });
	}
}

function turn(run: string, fence = 1): Turn {
	return {
		service: 'test',
		run,
		workflow: 'echo',
		params: {},
		asked_at: '2026-10-14T12:00:00Z',
		fence,
		expires_at: new Date(Date.now() + 30_000).toISOString(),
		done: {},
		partial: false,
	};
}

let ledger: Ledger;
let runs: Runs;
let lines: string[];

function driver(options: Partial<DriverOptions> = {}): Driver {
	return new Driver({
		node: 'tyo',
		capacity: 2,
		pushUrl: 'http://executor.invalid/turns',
		workflows: ['echo', 'nap'],
		ledger: ledger.fetch,
		start: runs.start,
		random: () => 0,
		log: (line) => lines.push(line),
		...options,
	});
}

beforeEach(() => {
	vi.useFakeTimers({ now: new Date('2026-10-14T12:00:00Z') });
	ledger = new Ledger();
	runs = new Runs();
	lines = [];
});

/** Stopped with no time given its turns: the deadline is a timer, so the clock moves for it. */
async function stopped(...drivers: Driver[]): Promise<void> {
	const stopping = Promise.all(drivers.map((at) => at.stop(0)));
	await vi.advanceTimersByTimeAsync(0);
	await stopping;
}

afterEach(() => {
	vi.useRealTimers();
});

describe('the driver', () => {
	it('polls at once and then every 15 to 30 seconds, naming what it can run', async () => {
		const at = driver({ random: () => 1 });
		at.start();
		await vi.advanceTimersByTimeAsync(0);
		expect(ledger.polls).toEqual([
			{ capacity: 2, push_url: 'http://executor.invalid/turns', workflows: ['echo', 'nap'] },
		]);
		await vi.advanceTimersByTimeAsync(29_999);
		expect(ledger.polls).toHaveLength(1);
		await vi.advanceTimersByTimeAsync(1);
		expect(ledger.polls).toHaveLength(2);
		const soon = driver({ random: () => 0 });
		soon.start();
		await vi.advanceTimersByTimeAsync(15_000);
		expect(ledger.polls).toHaveLength(4);
		await stopped(at, soon);
	});

	it('polls when the doorbell rings, once more for a ring during a poll', async () => {
		const at = driver();
		await Promise.all([at.ring(), at.ring(), at.ring()]);
		await vi.advanceTimersByTimeAsync(0);
		expect(ledger.polls).toHaveLength(2);
		await stopped(at);
	});

	it('begins each turn once however often a poll answers it, and keeps to its capacity', async () => {
		ledger.turns = [turn('a'), turn('b'), turn('c')];
		const at = driver();
		await at.ring();
		await at.ring();
		expect(runs.started.map((started) => started.run)).toEqual(['a', 'b']);
		expect(at.running).toBe(2);
		runs.end('a');
		await vi.advanceTimersByTimeAsync(0);
		ledger.turns = [turn('b'), turn('c')];
		await at.ring();
		expect(runs.started.map((started) => started.run)).toEqual(['a', 'b', 'c']);
		await stopped(at);
	});

	it('stops a turn when the run is granted again under a newer fence, and begins that', async () => {
		ledger.turns = [turn('a', 1)];
		const at = driver();
		await at.ring();
		ledger.turns = [turn('a', 2)];
		await at.ring();
		expect(runs.terminated).toEqual(['a@1']);
		expect(runs.started.map((started) => started.fence)).toEqual([1, 2]);
		await stopped(at);
	});

	it('renews every lease it holds every 10 seconds, under its node and fence', async () => {
		ledger.turns = [turn('a', 3)];
		const at = driver();
		await at.ring();
		await vi.advanceTimersByTimeAsync(RENEW_MS * 3);
		expect(ledger.renewals).toEqual(
			Array.from({ length: 3 }, () => ({ path: '/v1/runs/test/a/lease', node: 'tyo', fence: 3 })),
		);
		runs.end('a', 3);
		await vi.advanceTimersByTimeAsync(RENEW_MS * 2);
		expect(ledger.renewals).toHaveLength(3);
		await stopped(at);
	});

	it('stops a turn whose renewal is refused', async () => {
		ledger.turns = [turn('a')];
		const at = driver();
		await at.ring();
		ledger.renewal = 'stale';
		await vi.advanceTimersByTimeAsync(RENEW_MS);
		expect(runs.terminated).toEqual(['a@1']);
		expect(at.running).toBe(0);
		await stopped(at);
	});

	it('keeps a turn through a renewal not answered, and stops it once its lease has run out', async () => {
		ledger.turns = [turn('a')];
		const at = driver();
		await at.ring();
		ledger.renewal = 'down';
		await vi.advanceTimersByTimeAsync(RENEW_MS * 2);
		expect(runs.terminated).toEqual([]);
		await vi.advanceTimersByTimeAsync(RENEW_MS * 2);
		expect(runs.terminated).toEqual(['a@1']);
		await stopped(at);
	});

	it('stops polling when stopped, lets its turns end in time, and terminates the rest', async () => {
		ledger.turns = [turn('a'), turn('b')];
		const at = driver();
		at.start();
		await vi.advanceTimersByTimeAsync(0);
		const stopping = at.stop(15_000);
		await vi.advanceTimersByTimeAsync(1_000);
		runs.end('a');
		await vi.advanceTimersByTimeAsync(14_000);
		await stopping;
		expect(runs.terminated).toEqual(['b@1']);
		const polled = ledger.polls.length;
		await at.ring();
		await vi.advanceTimersByTimeAsync(60_000);
		expect(ledger.polls).toHaveLength(polled);
	});
});
