/** Answers its params from one step: the smallest run that proves a node replays a turn. */

import { WorkflowEntrypoint, type WorkflowEvent, type WorkflowStep } from 'cloudflare:workers';

export class Echo extends WorkflowEntrypoint {
	override async run(event: Readonly<WorkflowEvent<unknown>>, step: WorkflowStep) {
		return step.do('echo', async () => ({ echoed: event.payload, at: event.timestamp }));
	}
}
