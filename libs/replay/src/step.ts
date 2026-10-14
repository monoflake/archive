/**
 * Workflows' `step`, answered from the turn where a step is done and asked of the ledger where it
 * is not. A step that ends the turn returns a promise that never settles, so no `try` in the
 * workflow can catch the halt. See RFC 23wgz, "The second slice, the executor".
 */

import type {
	WorkflowSleepDuration,
	WorkflowStep,
	WorkflowStepConfig,
	WorkflowStepContext,
	WorkflowStepEvent,
	WorkflowTimeoutDuration,
} from 'cloudflare:workers';
import { NonRetryableError } from 'cloudflare:workflows';
import { DEFAULT_WAIT_MS, milliseconds, wire } from './config.ts';
import { type Answer, type RunClient, Unreachable, instant, isRecord } from './ledger.ts';

/** Why a turn ended before the workflow did. */
export type Halt =
	| { why: 'turn' }
	| { why: 'stale' }
	| { why: 'unreachable'; detail: string }
	| { why: 'diverged'; detail: string };

/** Thrown inside a step's own work to end the turn; never reaches the workflow. */
class Stop {
	constructor(readonly halt: Halt) {}
}

/** Picks a unique value out of a race with a step's callback. */
const TIMED_OUT = Symbol('timed out');

type Attempted = { ok: true; result: unknown } | { ok: false; error: string; fatal: boolean };

type Callback = (ctx: WorkflowStepContext) => Promise<unknown>;

export class Steps implements WorkflowStep {
	readonly #client: RunClient;
	readonly #done: Readonly<Record<string, unknown>>;
	readonly #counts = new Map<string, number>();
	#inFlight = 0;
	#halt: Halt | undefined;
	#onIdle!: (halt: Halt) => void;
	/** Settles once a step has ended the turn and no other step is still asking the ledger. */
	readonly idle: Promise<Halt>;

	constructor(client: RunClient, done: Readonly<Record<string, unknown>>) {
		this.#client = client;
		this.#done = done;
		this.idle = new Promise((resolve) => {
			this.#onIdle = resolve;
		});
	}

	get halted(): boolean {
		return this.#halt !== undefined;
	}

	do<T>(name: string, ...rest: unknown[]): Promise<T> {
		const [config, callback, rollback] = (
			typeof rest[0] === 'function' ? [undefined, rest[0], rest[1]] : rest
		) as [WorkflowStepConfig | undefined, Callback, unknown];
		const count = this.#count(name);
		if (rollback !== undefined) {
			return Promise.reject(new TypeError('a rollback is not supported by this runner'));
		}
		if (Object.hasOwn(this.#done, name)) return Promise.resolve(this.#done[name] as T);
		let wired: ReturnType<typeof wire>;
		try {
			wired = wire(config);
		} catch (error) {
			return Promise.reject(error);
		}
		const path = `/steps/${encodeURIComponent(name)}`;
		return this.#guard(async () => {
			const begun = this.#answered(
				await this.#client.call('POST', `${path}/attempts`, { config: wired }),
			);
			if (begun.status === 'done') return begun.result as T;
			if (begun.status === 'failed') throw new Error(String(begun.error));
			const attempt = Number(begun.attempt);
			const ctx = { step: { name, count }, attempt, config: config ?? {} };
			const ran = await attempted(callback, ctx, wired.timeout_ms);
			const body = ran.ok
				? { attempt, outcome: 'success', result: ran.result }
				: { attempt, outcome: ran.fatal ? 'fatal' : 'error', error: ran.error };
			const answer = await this.#client.call('PUT', path, body);
			if (!answer.ok && answer.code === 'result_too_large') throw new Error(answer.message);
			const finished = this.#answered(answer);
			if (finished.next === 'continue' && ran.ok) return ran.result as T;
			if (finished.next === 'retry') throw new Stop({ why: 'turn' });
			if (finished.next === 'fail') throw new Error(String(finished.error));
			throw new Stop({
				why: 'unreachable',
				detail: `the ledger answered next ${String(finished.next)}`,
			});
		});
	}

	sleep(name: string, duration: WorkflowSleepDuration): Promise<void> {
		try {
			const ms = milliseconds(duration);
			return this.#sleep(name, () => new Date(Date.now() + ms));
		} catch (error) {
			return Promise.reject(error);
		}
	}

	sleepUntil(name: string, timestamp: Date | number): Promise<void> {
		const wake = new Date(timestamp);
		if (Number.isNaN(wake.getTime())) {
			return Promise.reject(new TypeError(`${String(timestamp)} is not an instant`));
		}
		return this.#sleep(name, () => wake);
	}

	waitForEvent<T>(
		name: string,
		options: { type: string; timeout?: WorkflowTimeoutDuration | number },
	): Promise<WorkflowStepEvent<T>> {
		this.#count(name);
		if (Object.hasOwn(this.#done, name)) return Promise.resolve(taken<T>(this.#done[name]));
		let timeout: number;
		try {
			timeout = milliseconds(options.timeout ?? DEFAULT_WAIT_MS);
		} catch (error) {
			return Promise.reject(error);
		}
		return this.#guard(async () => {
			const body = { name, type: options.type, timeout_ms: timeout };
			const waited = this.#answered(await this.#client.call('POST', '/waits', body));
			if (waited.status === 'done') return taken<T>(waited.event);
			if (waited.status === 'failed') throw new Error(String(waited.error));
			throw new Stop({ why: 'turn' });
		});
	}

	#sleep(name: string, wake: () => Date): Promise<void> {
		this.#count(name);
		if (Object.hasOwn(this.#done, name)) return Promise.resolve();
		return this.#guard(async () => {
			const body = { name, wake_at: wake().toISOString() };
			const slept = this.#answered(await this.#client.call('POST', '/sleeps', body));
			if (slept.status === 'done') return;
			throw new Stop({ why: 'turn' });
		});
	}

	/** Which time this name is asked for in the turn, from 1: Workflows' `ctx.step.count`. */
	#count(name: string): number {
		const count = (this.#counts.get(name) ?? 0) + 1;
		this.#counts.set(name, count);
		return count;
	}

	/** A write's `data`, or the turn's end for a refusal no step answers for. */
	#answered(answer: Answer): Record<string, unknown> {
		if (answer.ok) return answer.data;
		if (answer.code === 'stale_lease') throw new Stop({ why: 'stale' });
		if (answer.code === 'replay_diverged')
			throw new Stop({ why: 'diverged', detail: answer.message });
		throw new Stop({ why: 'unreachable', detail: `${answer.status} ${answer.code}` });
	}

	/**
	 * Runs one step's work against the ledger, counted while it is in flight. Once the turn has
	 * ended, nothing more is asked, and every step that would have asked never settles.
	 */
	async #guard<T>(work: () => Promise<T>): Promise<T> {
		if (this.#halt) return never();
		this.#inFlight += 1;
		try {
			return await work();
		} catch (error) {
			if (error instanceof Stop) {
				this.#halt ??= error.halt;
				return never();
			}
			if (error instanceof Unreachable) {
				this.#halt ??= { why: 'unreachable', detail: error.message };
				return never();
			}
			throw error;
		} finally {
			this.#inFlight -= 1;
			if (this.#halt && this.#inFlight === 0) this.#onIdle(this.#halt);
		}
	}
}

function never<T>(): Promise<T> {
	return new Promise<T>(() => {});
}

/** The callback's attempt, its result as JSON reads it back, or why it failed. */
async function attempted(
	callback: Callback,
	ctx: WorkflowStepContext,
	timeout: number,
): Promise<Attempted> {
	let timer: ReturnType<typeof setTimeout> | undefined;
	const timedOut = new Promise<typeof TIMED_OUT>((resolve) => {
		timer = setTimeout(() => resolve(TIMED_OUT), timeout);
	});
	try {
		const value = await Promise.race([callback(ctx), timedOut]);
		if (value === TIMED_OUT) {
			return { ok: false, error: `the step timed out after ${timeout} ms`, fatal: false };
		}
		return { ok: true, result: value === undefined ? null : JSON.parse(JSON.stringify(value)) };
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		return { ok: false, error: message, fatal: error instanceof NonRetryableError };
	} finally {
		clearTimeout(timer);
	}
}

/** An event the ledger kept, `{type, payload, at}`, in Workflows' shape. */
function taken<T>(event: unknown): WorkflowStepEvent<T> {
	if (!isRecord(event)) throw new Unreachable('a taken event is an object');
	return { payload: event.payload as T, timestamp: instant(event.at), type: String(event.type) };
}
