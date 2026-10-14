/**
 * A ledger in memory for the runner's tests: one run's routes under `/v1`, deciding as
 * apps/data/ledger/src/store/runs does, in the small. Writes are recorded so a test can read what
 * the runner said.
 */

import type { LedgerFetch } from './ledger.ts';
import type { Turn } from './replay.ts';

type Step = {
	kind: 'do' | 'sleep' | 'wait';
	status: 'started' | 'retrying' | 'done' | 'failed' | 'sleeping' | 'waiting';
	attempt: number;
	limit: number;
	result?: unknown;
	error?: string;
	wakeAt?: string;
};

export type Written = { method: string; path: string; body: Record<string, unknown> };

const MAX_RESULT = 1 << 20;
const ASKED_AT = '2026-10-14T12:00:00.123456789Z';

export class FakeLedger {
	readonly service = 'store';
	readonly run = 'mirror-1';
	readonly steps = new Map<string, Step>();
	readonly events: { type: string; payload: unknown; at: string; taken: boolean }[] = [];
	readonly written: Written[] = [];
	fence = 1;
	holder = 'tyo';
	outcome: Record<string, unknown> | undefined;
	/** A route that answers no envelope: the ledger not reached. */
	down = false;

	constructor(
		readonly workflow = 'mirror',
		readonly params: unknown = { backup: 'base_1' },
	) {}

	/** The turn the ledger would hand out now, its done steps answered. */
	turn(): Turn {
		const done: Record<string, unknown> = {};
		for (const [name, step] of this.steps) {
			if (step.status === 'done') done[name] = step.kind === 'sleep' ? null : (step.result ?? null);
		}
		return {
			service: this.service,
			run: this.run,
			workflow: this.workflow,
			params: this.params,
			asked_at: ASKED_AT,
			fence: this.fence,
			expires_at: '2026-10-14T12:00:30Z',
			done,
			partial: false,
		};
	}

	/** The next turn, under a fence one higher, as a grant after a release. */
	next(): Turn {
		this.fence += 1;
		return this.turn();
	}

	/** Every sleep and wait due, as `due` makes them in the ledger's own tests. */
	due(): void {
		for (const step of this.steps.values()) {
			if (step.status === 'sleeping') step.status = 'done';
		}
	}

	send(type: string, payload: unknown): void {
		this.events.push({ type, payload, at: '2026-10-14T12:01:00.5Z', taken: false });
	}

	writtenTo(tail: string): Written[] {
		return this.written.filter((write) => write.path.endsWith(tail));
	}

	readonly fetch: LedgerFetch = async (path, init) => {
		if (this.down) throw new TypeError('fetch failed');
		const body = JSON.parse(String(init.body)) as Record<string, unknown>;
		const method = init.method ?? 'GET';
		this.written.push({ method, path, body });
		const prefix = `/v1/runs/${this.service}/${this.run}`;
		if (!path.startsWith(prefix)) return refused(404, 'no_such_run');
		if (body.node !== this.holder || body.fence !== this.fence) return refused(409, 'stale_lease');
		const tail = path.slice(prefix.length);
		const stepped = /^\/steps\/([^/]+)(\/attempts)?$/.exec(tail);
		if (stepped && stepped[2]) return this.#begin(decodeURIComponent(stepped[1] as string), body);
		if (stepped) return this.#finish(decodeURIComponent(stepped[1] as string), body);
		if (tail === '/sleeps') return this.#sleep(body);
		if (tail === '/waits') return this.#wait(body);
		if (tail === '/outcome') {
			this.outcome = body;
			return ok({});
		}
		return refused(404, 'no_such_route');
	};

	#kind(name: string, kind: Step['kind']): Step | Response | undefined {
		const step = this.steps.get(name);
		if (step && step.kind !== kind)
			return refused(409, 'replay_diverged', `${name} is a ${step.kind}`);
		return step;
	}

	#begin(name: string, body: Record<string, unknown>): Response {
		const found = this.#kind(name, 'do');
		if (found instanceof Response) return found;
		if (found?.status === 'done') return ok({ status: 'done', result: found.result });
		if (found?.status === 'failed') return ok({ status: 'failed', error: found.error });
		const config = body.config as { retries: { limit: number } };
		const step = found ?? {
			kind: 'do',
			status: 'started',
			attempt: 0,
			limit: config.retries.limit,
		};
		step.status = 'started';
		step.attempt += 1;
		this.steps.set(name, step);
		return ok({ status: 'started', attempt: step.attempt, deadline: '2026-10-14T12:05:00Z' });
	}

	#finish(name: string, body: Record<string, unknown>): Response {
		const step = this.steps.get(name);
		if (!step || step.status !== 'started')
			return refused(409, 'replay_diverged', `${name} is not begun`);
		if (body.outcome === 'success') {
			if (JSON.stringify(body.result).length > MAX_RESULT) {
				Object.assign(step, { status: 'failed', error: 'the result is larger than 1 MiB' });
				return refused(413, 'result_too_large', 'the result is larger than 1 MiB');
			}
			Object.assign(step, { status: 'done', result: body.result });
			return ok({ next: 'continue' });
		}
		if (body.outcome === 'error' && step.attempt <= step.limit) {
			step.status = 'retrying';
			this.holder = '';
			return ok({ next: 'retry', run_at: '2026-10-14T12:00:10Z' });
		}
		Object.assign(step, { status: 'failed', error: String(body.error) });
		return ok({ next: 'fail', error: String(body.error) });
	}

	#sleep(body: Record<string, unknown>): Response {
		const name = String(body.name);
		const found = this.#kind(name, 'sleep');
		if (found instanceof Response) return found;
		if (found?.status === 'done') return ok({ status: 'done' });
		const step = found ?? {
			kind: 'sleep',
			status: 'sleeping',
			attempt: 0,
			limit: 0,
			wakeAt: String(body.wake_at),
		};
		this.steps.set(name, step);
		this.holder = '';
		return ok({ status: 'sleeping', wake_at: step.wakeAt });
	}

	#wait(body: Record<string, unknown>): Response {
		const name = String(body.name);
		const found = this.#kind(name, 'wait');
		if (found instanceof Response) return found;
		if (found?.status === 'done') return ok({ status: 'done', event: found.result });
		if (found?.status === 'failed') return ok({ status: 'failed', error: found.error });
		const event = this.events.find((sent) => sent.type === body.type && !sent.taken);
		if (event) {
			event.taken = true;
			const result = { type: event.type, payload: event.payload, at: event.at };
			this.steps.set(name, { kind: 'wait', status: 'done', attempt: 0, limit: 0, result });
			return ok({ status: 'done', event: result });
		}
		if (body.timeout_ms === 0) {
			const error = `no ${String(body.type)} event came in time`;
			this.steps.set(name, { kind: 'wait', status: 'failed', attempt: 0, limit: 0, error });
			return ok({ status: 'failed', error });
		}
		this.steps.set(name, { kind: 'wait', status: 'waiting', attempt: 0, limit: 0 });
		this.holder = '';
		return ok({ status: 'waiting', wake_at: '2026-10-15T12:00:00Z' });
	}
}

/** The holder a released run is granted to again, under `next()`'s fence. */
export function regrant(ledger: FakeLedger, node = 'tyo'): Turn {
	ledger.holder = node;
	return ledger.next();
}

function ok(data: Record<string, unknown>): Response {
	return Response.json({ status: 'success', data });
}

function refused(status: number, code: string, message = code): Response {
	return Response.json({ status: 'error', code, message }, { status });
}
