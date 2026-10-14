/**
 * The turn a thread replays, its steps counted for the driver's stall clock, and how it ended sent
 * back once. Loaded by thread.ts once the resolve hook is in place. See
 * spec/architecture/executor.md, "A thread stands in for an isolate".
 */

import { parentPort, workerData } from 'node:worker_threads';
import type { WorkflowEvent, WorkflowStep } from 'cloudflare:workers';
import { type Entrypoint, replay } from '@monoflake/replay';
import { envOf, type Fetcher } from './bindings.ts';
import { ledgerAt } from './ledger.ts';
import type { TurnData } from './turns.ts';

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
