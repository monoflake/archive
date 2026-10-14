/**
 * The workflows the end-to-end tests open runs of, each reporting what it does to the `WITNESS`
 * binding so a test can count what ran, in whichever thread it ran.
 */

import { WorkflowEntrypoint, type WorkflowEvent, type WorkflowStep } from 'cloudflare:workers';
import { NonRetryableError } from 'cloudflare:workflows';

type Env = { WITNESS: { fetch(path: string): Promise<Response> } };

abstract class Witnessed extends WorkflowEntrypoint<Env> {
	protected async saw(path: string): Promise<void> {
		await (await this.env.WITNESS.fetch(path)).text();
	}
}

/** A step, an hour's sleep, and a step that reads the first one's result. */
export class Issue extends Witnessed {
	override async run(_: unknown, step: WorkflowStep) {
		const order = await step.do('order', async () => {
			await this.saw('/order');
			return { order: 'o-1' };
		});
		await step.sleep('propagate', '1 hour');
		return step.do('keep', async () => {
			await this.saw('/keep');
			return { kept: order.order };
		});
	}
}

/** A step held at the witness until the test lets it go, under `/hold/<run>`; never retried. */
export class Hold extends Witnessed {
	override async run(event: Readonly<WorkflowEvent<unknown>>, step: WorkflowStep) {
		return step.do('hold', { retries: { limit: 0, delay: 1 } }, async () => {
			await this.saw(`/hold/${event.instanceId}`);
			return 'held';
		});
	}
}

/** A step that always fails, retried twice an hour apart. */
export class Flaky extends Witnessed {
	override async run(_: unknown, step: WorkflowStep) {
		return step.do('flaky', { retries: { limit: 2, delay: '1 hour' } }, async () => {
			await this.saw('/flaky');
			throw new Error('the store said no');
		});
	}
}

export class Fatal extends Witnessed {
	override async run(_: unknown, step: WorkflowStep) {
		return step.do('doomed', async () => {
			await this.saw('/doomed');
			throw new NonRetryableError('never');
		});
	}
}

export class Huge extends WorkflowEntrypoint {
	override async run(_: unknown, step: WorkflowStep) {
		return step.do('huge', async () => 'x'.repeat(1 << 20));
	}
}

/** `same` as a step that is retried an hour later. */
export class Before extends WorkflowEntrypoint {
	override async run(_: unknown, step: WorkflowStep) {
		return step.do('same', { retries: { limit: 1, delay: '1 hour' } }, async () => {
			throw new Error('again');
		});
	}
}

/** `same` as a sleep: what newer code that disagrees with the record looks like. */
export class After extends WorkflowEntrypoint {
	override async run(_: unknown, step: WorkflowStep) {
		await step.sleep('same', '1 hour');
	}
}

/** Waits an hour for an approval, and says when none came. */
export class Approve extends WorkflowEntrypoint {
	override async run(_: unknown, step: WorkflowStep) {
		try {
			const approved = await step.waitForEvent<{ by: string }>('approved', {
				type: 'approval',
				timeout: '1 hour',
			});
			return { by: approved.payload.by, type: approved.type };
		} catch (error) {
			return { unanswered: (error as Error).message };
		}
	}
}
