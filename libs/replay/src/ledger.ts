/**
 * The run's routes on the ledger, under `/v1`, as one holder of one turn calls them. See
 * repos/platform/spec/architecture/ledger.md, "The routes".
 */

/**
 * How a path on the ledger is reached: the driver's own fetch to the private side under Node, a
 * service binding under workerd. `path` starts at `/v1`.
 */
export type LedgerFetch = (path: string, init: RequestInit) => Promise<Response>;

/** What a write answered: its `data`, or the code it was refused with. */
export type Answer =
	| { ok: true; data: Record<string, unknown> }
	| { ok: false; status: number; code: string; message: string };

/** The ledger could not be asked, or answered something that is not its envelope. */
export class Unreachable extends Error {
	override name = 'Unreachable';
}

export class RunClient {
	readonly #ledger: LedgerFetch;
	readonly #base: string;
	readonly #holder: { node: string; fence: number };

	constructor(ledger: LedgerFetch, service: string, run: string, node: string, fence: number) {
		this.#ledger = ledger;
		this.#base = `/v1/runs/${encodeURIComponent(service)}/${encodeURIComponent(run)}`;
		this.#holder = { node, fence };
	}

	/** `tail` under the run, the holder's `node` and `fence` added to `body`. */
	async call(method: string, tail: string, body: Record<string, unknown>): Promise<Answer> {
		let response: Response;
		try {
			response = await this.#ledger(`${this.#base}${tail}`, {
				method,
				headers: { 'content-type': 'application/json' },
				body: JSON.stringify({ ...this.#holder, ...body }),
			});
		} catch (error) {
			throw new Unreachable(`the ledger was not reached: ${String(error)}`);
		}
		let envelope: unknown;
		try {
			envelope = await response.json();
		} catch {
			throw new Unreachable(`the ledger answered ${response.status} with no envelope`);
		}
		if (!isRecord(envelope))
			throw new Unreachable(`the ledger answered ${response.status} with no envelope`);
		if (envelope.status === 'success' && isRecord(envelope.data)) {
			return { ok: true, data: envelope.data };
		}
		if (envelope.status === 'error' && typeof envelope.code === 'string') {
			const message = typeof envelope.message === 'string' ? envelope.message : envelope.code;
			return { ok: false, status: response.status, code: envelope.code, message };
		}
		throw new Unreachable(`the ledger answered ${response.status} with no envelope`);
	}
}

export function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** An RFC 3339 instant the ledger wrote, to nanoseconds, as a `Date`, to milliseconds. */
export function instant(written: unknown): Date {
	if (typeof written !== 'string')
		throw new Unreachable(`an instant is a string, not ${String(written)}`);
	const date = new Date(written.replace(/(\.\d{3})\d+/, '$1'));
	if (Number.isNaN(date.getTime())) throw new Unreachable(`"${written}" is not an instant`);
	return date;
}
