/**
 * One turn in a thread of its own, the stand-in for workerd's isolate per call: a fresh global
 * scope, a heap of its own, and stopped by `terminate()`. It answers how the turn ended once, by
 * message. See spec/architecture/executor.md, "A thread stands in for an isolate".
 */

import { registerHooks } from 'node:module';
import { parentPort, workerData } from 'node:worker_threads';
import type { WorkflowEvent, WorkflowStep } from 'cloudflare:workers';
import type { Entrypoint } from '@monoflake/replay';
import type { Fetcher } from './bindings.ts';
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

/** Sent to the driver: a thread's port, not a window's, so it has no origin to name. */
function tell(message: unknown): void {
	// oxlint-disable-next-line unicorn/require-post-message-target-origin
	parentPort?.postMessage(message);
}

let asking = 0;

/**
 * A step's call counted while it is out, so the driver's stall clock runs only while none is: a
 * step that ends the turn never settles, which leaves it counted, and the turn ends anyway.
 */
function counted<T>(call: Promise<T>): Promise<T> {
	if (asking++ === 0) tell({ busy: true });
	return call.finally(() => {
		if (--asking === 0) tell({ busy: false });
	});
}

/** `step` as the runner made it, each call counted. */
function watched(step: WorkflowStep): WorkflowStep {
	return {
		do: (...args: unknown[]) =>
			counted((step.do as (...all: unknown[]) => Promise<unknown>)(...args)),
		sleep: (name, duration) => counted(step.sleep(name, duration)),
		sleepUntil: (name, timestamp) => counted(step.sleepUntil(name, timestamp)),
		waitForEvent: (name, options) => counted(step.waitForEvent(name, options)),
	} as WorkflowStep;
}

/** The turn replayed, its end sent back; a turn terminated mid-way never sends one. */
async function turn(data: TurnData): Promise<void> {
	const { replay } = await import('@monoflake/replay');
	const { envOf } = await import('./bindings.ts');
	const { ledgerAt } = await import('./ledger.ts');
	const code: Entrypoint<Record<string, Fetcher>> = (await import(data.workflow.module))[
		data.workflow.export
	];
	const entrypoint = class extends code {
		override run(event: Readonly<WorkflowEvent<unknown>>, step: WorkflowStep) {
			// The workflow's own `run`, as its class declared it, given the counted `step`.
			return code.prototype.run.call(this, event, watched(step));
		}
	};
	tell(
		await replay({
			turn: data.turn,
			node: data.node,
			entrypoint,
			env: envOf(data.bindings),
			ledger: ledgerAt(data.ledger),
		}),
	);
}

void turn(workerData as TurnData);
