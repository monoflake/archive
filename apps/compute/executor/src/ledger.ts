/**
 * The executor's own two routes on the ledger: its poll, and a lease's renewal. A turn's writes
 * are the runner's, in libs/replay. See spec/architecture/ledger.md, "The routes".
 */

import type { LedgerFetch, Turn } from '@monoflake/replay';

/** What the ledger answered: its `data`, the code it refused with, or nothing it could read. */
export type Answer =
	| { ok: true; data: Record<string, unknown> }
	| { ok: false; code: string; detail: string };

/** A ledger at `base` on the private side, reached by `fetch`. */
export function ledgerAt(base: string, reach: typeof fetch = fetch): LedgerFetch {
	return (path, init) => reach(`${base.replace(/\/$/, '')}${path}`, init);
}

export async function ask(
	ledger: LedgerFetch,
	method: string,
	path: string,
	body: Record<string, unknown>,
): Promise<Answer> {
	let envelope: unknown;
	let status = 0;
	try {
		const response = await ledger(path, {
			method,
			headers: { 'content-type': 'application/json' },
			body: JSON.stringify(body),
		});
		status = response.status;
		envelope = await response.json();
	} catch (error) {
		return { ok: false, code: 'unreachable', detail: String(error) };
	}
	const answer = (envelope ?? {}) as Record<string, unknown>;
	if (answer.status === 'success' && typeof answer.data === 'object' && answer.data !== null) {
		return { ok: true, data: answer.data as Record<string, unknown> };
	}
	const code = typeof answer.code === 'string' ? answer.code : 'unreachable';
	return { ok: false, code, detail: `${status} ${code}` };
}

/** The turns a poll answered; a turn the runner could not read is left for the next poll. */
export function turnsOf(data: Record<string, unknown>): Turn[] {
	const turns = Array.isArray(data.turns) ? data.turns : [];
	return turns.filter(
		(turn): turn is Turn =>
			typeof turn === 'object' &&
			turn !== null &&
			typeof turn.service === 'string' &&
			typeof turn.run === 'string' &&
			typeof turn.workflow === 'string' &&
			typeof turn.fence === 'number',
	);
}
