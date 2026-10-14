/**
 * Workflows' durations and step config, read into what the ledger keeps: milliseconds, and a
 * config whole, its missing parts taken from Workflows' defaults.
 */

import type { WorkflowSleepDuration, WorkflowStepConfig } from 'cloudflare:workers';

/** A step's callback has at most this long, l4s6a's limit, already under Node. RFC 23wgz. */
export const CALLBACK_LIMIT_MS = 300_000;

/** Workflows' defaults: https://developers.cloudflare.com/workflows/build/sleeping-and-retrying/ */
const DEFAULT_LIMIT = 5;
const DEFAULT_DELAY_MS = 10_000;
const DEFAULT_TIMEOUT_MS = 600_000;
/** Workflows' own limit on a step's retries. */
const MAX_LIMIT = 10_000;

/** `waitForEvent` waits a day unless told otherwise, as Workflows does. */
export const DEFAULT_WAIT_MS = 86_400_000;

const UNIT_MS: Record<string, number> = {
	second: 1_000,
	minute: 60_000,
	hour: 3_600_000,
	day: 86_400_000,
	week: 604_800_000,
	month: 2_592_000_000,
	year: 31_536_000_000,
};

const SPOKEN = /^(\d+(?:\.\d+)?) (second|minute|hour|day|week|month|year)s?$/;

/** A duration in milliseconds: a number is one already, a string is `<n> <unit>[s]`. */
export function milliseconds(duration: WorkflowSleepDuration | number): number {
	if (typeof duration === 'number') {
		if (!Number.isFinite(duration) || duration < 0) {
			throw new TypeError(`a duration is a finite number of milliseconds, not ${duration}`);
		}
		return Math.round(duration);
	}
	const spoken = SPOKEN.exec(duration);
	if (!spoken) throw new TypeError(`a duration is "<n> <unit>", not "${duration}"`);
	return Math.round(Number(spoken[1]) * (UNIT_MS[spoken[2] as string] as number));
}

/** A step's config as `POST .../attempts` takes it. */
export type WireConfig = {
	retries: { limit: number; delay_ms: number; backoff: 'constant' | 'linear' | 'exponential' };
	timeout_ms: number;
};

/**
 * The config the ledger keeps, the timeout held to `CALLBACK_LIMIT_MS`: Workflows' default of 10
 * minutes reads as 5. A delay given as a function is refused, since no number can stand for it.
 */
export function wire(config: WorkflowStepConfig = {}): WireConfig {
	const retries = config.retries;
	if (retries && typeof retries.delay === 'function') {
		throw new TypeError('a delay given as a function is not supported by this runner');
	}
	const limit = retries?.limit ?? DEFAULT_LIMIT;
	if (!Number.isInteger(limit) || limit < 0 || limit > MAX_LIMIT) {
		throw new TypeError(`retries.limit is a whole number from 0 to ${MAX_LIMIT}, not ${limit}`);
	}
	const timeout = milliseconds(config.timeout ?? DEFAULT_TIMEOUT_MS);
	return {
		retries: {
			limit,
			delay_ms: retries ? milliseconds(retries.delay) : DEFAULT_DELAY_MS,
			backoff: retries?.backoff ?? 'exponential',
		},
		timeout_ms: Math.max(1, Math.min(timeout, CALLBACK_LIMIT_MS)),
	};
}
