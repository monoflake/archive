/** Workflows the driver's tests alone run, each proving what a thread holds a turn to. */

import { WorkflowEntrypoint, type WorkflowStep } from 'cloudflare:workers';

/** Counts on the global scope: a fresh scope per turn counts 1 every time. */
export class Counter extends WorkflowEntrypoint {
	override async run() {
		const scope = globalThis as { counted?: number };
		scope.counted = (scope.counted ?? 0) + 1;
		return { counted: scope.counted };
	}
}

/** Holds far more than a turn's heap may. */
export class Hog extends WorkflowEntrypoint {
	override async run() {
		const held: number[][] = [];
		for (;;) held.push(Array.from({ length: 1_000_000 }, (_, index) => index));
	}
}

/** Never returns and never asks the ledger: only `terminate()` ends it. */
export class Hang extends WorkflowEntrypoint {
	override run(): Promise<unknown> {
		return new Promise(() => {});
	}
}

/** Reaches the registry through its binding, as a workflow reaches any service. */
export class Bound extends WorkflowEntrypoint<{
	REGISTRY: { fetch(path: string): Promise<Response> };
}> {
	override async run(_: unknown, step: WorkflowStep) {
		return step.do('placements', async () =>
			(await this.env.REGISTRY.fetch('/v1/apps/store')).json(),
		);
	}
}
