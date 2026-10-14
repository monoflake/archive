/**
 * One turn in a thread of its own, the stand-in for workerd's isolate per call: a fresh global
 * scope, a heap of its own, and stopped by `terminate()`. It answers how the turn ended once, by
 * message. See spec/architecture/executor.md, "A thread stands in for an isolate".
 */

import { registerHooks } from 'node:module';
import { parentPort, workerData } from 'node:worker_threads';
import type { TurnData } from './turns.ts';

// What workerd provides by these names, the runner's own module stands in for; registered before
// anything that imports them is loaded, which is why everything below is imported late.
const shim = import.meta.resolve('@monoflake/replay/cloudflare');
registerHooks({
	resolve: (specifier, context, next) =>
		/^cloudflare:(workers|workflows)$/.test(specifier)
			? { url: shim, shortCircuit: true }
			: next(specifier, context),
});

/** The turn replayed, its end sent back; a turn terminated mid-way never sends one. */
async function turn(data: TurnData): Promise<void> {
	const { replay } = await import('@monoflake/replay');
	const { envOf } = await import('./bindings.ts');
	const { ledgerAt } = await import('./ledger.ts');
	const entrypoint = (await import(data.workflow.module))[data.workflow.export];
	const ended = await replay({
		turn: data.turn,
		node: data.node,
		entrypoint,
		env: envOf(data.bindings),
		ledger: ledgerAt(data.ledger),
	});
	// A thread's port, not a window's: it has no origin to name.
	// oxlint-disable-next-line unicorn/require-post-message-target-origin
	parentPort?.postMessage(ended);
}

void turn(workerData as TurnData);
