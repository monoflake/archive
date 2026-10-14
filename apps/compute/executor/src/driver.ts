/**
 * The driver: polls the ledger for the turns this node holds, runs each in a thread of its own up
 * to its capacity, renews each lease while its turn runs, and stops a turn whose lease is no longer
 * its own. A push only rings for a poll. See spec/architecture/executor.md, "Polls, and a push
 * that is a doorbell".
 */

import type { Ended, LedgerFetch, Turn } from '@monoflake/replay';
import { ask, turnsOf } from './ledger.ts';

/** A turn under way, and how to stop it. */
export interface Running {
	readonly ended: Promise<Ended>;
	terminate(): void;
	/** Told when a step starts asking the ledger or running its callback, and when none is. */
	onBusy(listener: (busy: boolean) => void): void;
}

export type Start = (turn: Turn) => Running;

/** A lease is renewed this often, a third of its 30 seconds. See RFC 23wgz. */
export const RENEW_MS = 10_000;

/**
 * A turn may go this long with no step asking the ledger or running its callback; past it, it has
 * stalled. See spec/architecture/executor.md, "The limits".
 */
export const STALL_MS = 300_000;

/** A poll every 15 to 30 seconds, at random, so nodes started together do not poll together. */
export const POLL_MS: readonly [number, number] = [15_000, 30_000];

export interface DriverOptions {
	readonly node: string;
	readonly capacity: number;
	readonly pushUrl?: string;
	/** The workflows it can run, named in every poll. */
	readonly workflows: readonly string[];
	readonly ledger: LedgerFetch;
	readonly start: Start;
	readonly renewMs?: number;
	readonly stallMs?: number;
	readonly pollMs?: readonly [number, number];
	readonly random?: () => number;
	readonly log?: (line: string) => void;
}

interface Held {
	readonly turn: Turn;
	readonly running: Running;
	readonly renewal: ReturnType<typeof setInterval>;
	/** Runs while no step is asking the ledger or running its callback. */
	stall: ReturnType<typeof setTimeout> | undefined;
	/** When the lease runs out on the ledger's clock, as last told, in milliseconds. */
	expiresAt: number;
}

export class Driver {
	readonly #options: DriverOptions;
	readonly #log: (line: string) => void;
	/** Each turn under way, by its run. */
	readonly #held = new Map<string, Held>();
	#timer: ReturnType<typeof setTimeout> | undefined;
	#polling: Promise<void> | undefined;
	#again = false;
	#stopped = false;
	/** When a poll was last answered, for `/health`. */
	polledAt: Date | undefined;

	constructor(options: DriverOptions) {
		this.#options = options;
		this.#log = options.log ?? ((line) => console.log(`executor: ${line}`));
	}

	get running(): number {
		return this.#held.size;
	}

	/** Polls now, then every 15 to 30 seconds until stopped. */
	start(): void {
		void this.ring();
		this.#schedule();
	}

	/**
	 * Polls now: the doorbell, and the timer. A ring while a poll is out polls once more after it,
	 * so a grant made during a poll is not left for the next timer.
	 */
	ring(): Promise<void> {
		if (this.#stopped) return Promise.resolve();
		if (this.#polling) {
			this.#again = true;
			return this.#polling;
		}
		this.#polling = this.#poll().finally(() => {
			this.#polling = undefined;
			if (this.#again) {
				this.#again = false;
				void this.ring();
			}
		});
		return this.#polling;
	}

	/**
	 * Stops polling, lets each running turn end within `withinMs`, then terminates the rest. A turn
	 * cut off is a lost holder, taken under the next fence, which is the model.
	 */
	async stop(withinMs: number): Promise<void> {
		this.#stopped = true;
		clearTimeout(this.#timer);
		await this.#polling;
		const ending = [...this.#held.values()].map((held) => held.running.ended);
		let deadline: ReturnType<typeof setTimeout> | undefined;
		await Promise.race([
			Promise.allSettled(ending),
			new Promise((resolve) => {
				deadline = setTimeout(resolve, withinMs);
			}),
		]);
		clearTimeout(deadline);
		for (const held of this.#held.values()) this.#drop(held, 'stopping');
	}

	#schedule(): void {
		if (this.#stopped) return;
		const [least, most] = this.#options.pollMs ?? POLL_MS;
		const random = this.#options.random ?? Math.random;
		this.#timer = setTimeout(
			() => {
				void this.ring();
				this.#schedule();
			},
			least + random() * (most - least),
		);
	}

	async #poll(): Promise<void> {
		const { node, capacity, pushUrl, workflows, ledger } = this.#options;
		const body = { capacity, push_url: pushUrl ?? null, workflows };
		const answer = await ask(
			ledger,
			'POST',
			`/v1/executors/${encodeURIComponent(node)}/polls`,
			body,
		);
		if (!answer.ok) {
			this.#log(`a poll was not answered: ${answer.detail}`);
			return;
		}
		this.polledAt = new Date();
		for (const turn of turnsOf(answer.data)) this.#take(turn);
	}

	/** A turn the poll answered: begun once, a newer fence of a run replacing the older. */
	#take(turn: Turn): void {
		const key = `${turn.service}/${turn.run}`;
		const held = this.#held.get(key);
		if (held && held.turn.fence >= turn.fence) return;
		if (held) this.#drop(held, `fence ${turn.fence} granted since`);
		if (this.#held.size >= this.#options.capacity) {
			this.#log(
				`${key} is held past capacity ${this.#options.capacity}; left for its lease to end`,
			);
			return;
		}
		const running = this.#options.start(turn);
		const renewal = setInterval(() => void this.#renew(key), this.#options.renewMs ?? RENEW_MS);
		const expiresAt = Date.parse(turn.expires_at);
		const entry: Held = { turn, running, renewal, stall: undefined, expiresAt };
		this.#held.set(key, entry);
		// The clock starts with the turn, since the code runs from the top before any step asks.
		const idle = () => {
			clearTimeout(entry.stall);
			entry.stall = setTimeout(() => void this.#stalled(entry), this.#options.stallMs ?? STALL_MS);
		};
		idle();
		running.onBusy((busy) => (busy ? clearTimeout(entry.stall) : idle()));
		void running.ended.then((ended) => {
			clearInterval(renewal);
			clearTimeout(entry.stall);
			if (this.#held.get(key) === entry) this.#held.delete(key);
			const why =
				ended.ended === 'halted' ? ` (${ended.why}${ended.detail ? `: ${ended.detail}` : ''})` : '';
			this.#log(`${key} at fence ${turn.fence} ended ${ended.ended}${why}`);
		});
	}

	/** The lease renewed; refused, or run out with no renewal heard, the turn is stopped. */
	async #renew(key: string): Promise<void> {
		const held = this.#held.get(key);
		if (!held) return;
		const { service, run, fence } = held.turn;
		const path = `/v1/runs/${encodeURIComponent(service)}/${encodeURIComponent(run)}/lease`;
		const answer = await ask(this.#options.ledger, 'PUT', path, {
			node: this.#options.node,
			fence,
		});
		if (this.#held.get(key) !== held) return;
		if (answer.ok && typeof answer.data.expires_at === 'string') {
			held.expiresAt = Date.parse(answer.data.expires_at);
		} else if (!answer.ok && answer.code === 'stale_lease') {
			this.#drop(held, 'its renewal was refused');
		} else if (Date.now() > held.expiresAt) {
			this.#drop(held, 'its lease ran out with no renewal answered');
		}
	}

	/**
	 * A turn past its limit with no step asking: stopped, and told to the ledger under its lease,
	 * which makes the run ready for the next turn or fails it `turn_stalled`.
	 */
	async #stalled(held: Held): Promise<void> {
		const key = `${held.turn.service}/${held.turn.run}`;
		if (this.#held.get(key) !== held) return;
		this.#drop(held, 'it stalled');
		const { service, run, fence } = held.turn;
		const path = `/v1/runs/${encodeURIComponent(service)}/${encodeURIComponent(run)}/stalls`;
		const answer = await ask(this.#options.ledger, 'POST', path, {
			node: this.#options.node,
			fence,
		});
		const told = answer.ok ? `the run is ${String(answer.data.status)}` : answer.detail;
		this.#log(`${key} at fence ${fence} stalled: ${told}`);
	}

	#drop(held: Held, why: string): void {
		const key = `${held.turn.service}/${held.turn.run}`;
		clearInterval(held.renewal);
		clearTimeout(held.stall);
		if (this.#held.get(key) === held) this.#held.delete(key);
		held.running.terminate();
		this.#log(`${key} at fence ${held.turn.fence} stopped: ${why}`);
	}
}
