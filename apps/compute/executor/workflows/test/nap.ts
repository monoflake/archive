/** A step, a sleep of `seconds`, and a step: a run of two turns, holding nothing between. */

import { WorkflowEntrypoint, type WorkflowEvent, type WorkflowStep } from 'cloudflare:workers';

export class Nap extends WorkflowEntrypoint {
	override async run(event: Readonly<WorkflowEvent<{ seconds?: number }>>, step: WorkflowStep) {
		const before = await step.do('before', async () => Date.now());
		await step.sleep('nap', (event.payload.seconds ?? 1) * 1000);
		const after = await step.do('after', async () => Date.now());
		return { slept: after - before };
	}
}
