import { WorkflowEntrypoint, type WorkflowEvent, type WorkflowStep } from 'cloudflare:workers';
import { NonRetryableError } from 'cloudflare:workflows';
import { describe, expect, it } from 'vitest';
import { FakeLedger, regrant } from './fake-ledger.ts';
import { type Ended, replay } from './replay.ts';

type Code = (event: Readonly<WorkflowEvent<unknown>>, step: WorkflowStep) => Promise<unknown>;

/** A workflow class around `code`, as a workflow is written: a `WorkflowEntrypoint`. */
function workflow(code: Code) {
	return class extends WorkflowEntrypoint<object> {
		override run(event: Readonly<WorkflowEvent<unknown>>, step: WorkflowStep) {
			return code(event, step);
		}
	};
}

function turn(ledger: FakeLedger, code: Code, node = 'tyo', given = ledger.turn()): Promise<Ended> {
	return replay({ turn: given, node, entrypoint: workflow(code), env: {}, ledger: ledger.fetch });
}

const napping: Code = async (_, step) => {
	await step.sleep('nap', '1 hour');
	await step.sleepUntil('until', new Date('2026-10-14T13:00:00Z'));
};

const approval: Code = async (_, step) =>
	step.waitForEvent('approved', { type: 'approval', timeout: '1 hour' });

describe('a turn', () => {
	it('gives the code its run as Workflows does, the same instant in every turn', async () => {
		const ledger = new FakeLedger('mirror', { backup: 'base_7' });
		const seen: unknown[] = [];
		await turn(ledger, async (event) => seen.push(event));
		const [event] = seen as WorkflowEvent<unknown>[];
		expect(event?.payload).toEqual({ backup: 'base_7' });
		expect(event?.instanceId).toBe('mirror-1');
		expect(event?.workflowName).toBe('mirror');
		expect(event?.timestamp).toEqual(new Date('2026-10-14T12:00:00.123Z'));
	});

	it('answers a done step from the turn without running its callback', async () => {
		const ledger = new FakeLedger();
		let ran = 0;
		const code: Code = async (_, step) => {
			const order = await step.do('order', async () => {
				ran += 1;
				return { order: 'o-1' };
			});
			await step.sleep('propagate', '1 hour');
			return step.do('keep', async () => ({ kept: order.order }));
		};
		expect(await turn(ledger, code)).toEqual({ ended: 'halted', why: 'turn' });
		expect(ledger.outcome).toBeUndefined();
		ledger.due();
		expect(await turn(ledger, code, 'tyo', regrant(ledger))).toEqual({
			ended: 'done',
			output: { kept: 'o-1' },
		});
		expect(ran).toBe(1);
		expect(ledger.writtenTo('/steps/order/attempts')).toHaveLength(1);
		expect(ledger.outcome).toMatchObject({ outcome: 'done', output: { kept: 'o-1' } });
	});

	it('ends with a halt no try in the workflow can catch', async () => {
		const ledger = new FakeLedger();
		let caught = false;
		let after = false;
		const ended = await turn(ledger, async (_, step) => {
			try {
				await step.sleep('wait', '1 day');
			} catch {
				caught = true;
			}
			after = true;
		});
		expect(ended).toEqual({ ended: 'halted', why: 'turn' });
		expect({ caught, after }).toEqual({ caught: false, after: false });
	});

	it('throws a step that failed for good, which the code may catch and go on from', async () => {
		const ledger = new FakeLedger();
		const ended = await turn(ledger, async (_, step) => {
			try {
				await step.do('flaky', { retries: { limit: 0, delay: 1 } }, async () => {
					throw new Error('the store said no');
				});
			} catch (error) {
				return { recovered: (error as Error).message };
			}
		});
		expect(ended).toEqual({ ended: 'done', output: { recovered: 'the store said no' } });
		expect(ledger.writtenTo('/steps/flaky')[0]?.body).toMatchObject({
			outcome: 'error',
			error: 'the store said no',
		});
	});

	it('ends the turn on a retry, and reports a NonRetryableError as fatal', async () => {
		const ledger = new FakeLedger();
		const retried = await turn(ledger, async (_, step) =>
			step.do('flaky', async () => {
				throw new Error('try again');
			}),
		);
		expect(retried).toEqual({ ended: 'halted', why: 'turn' });
		const fatal = await turn(new FakeLedger(), async (_, step) =>
			step.do('doomed', async () => {
				throw new NonRetryableError('never');
			}),
		);
		expect(fatal).toEqual({ ended: 'failed', error: 'never' });
	});

	it('reports NonRetryableError as fatal to the ledger', async () => {
		const ledger = new FakeLedger();
		await turn(ledger, async (_, step) =>
			step.do('doomed', async () => {
				throw new NonRetryableError('never');
			}),
		);
		expect(ledger.writtenTo('/steps/doomed')[0]?.body).toMatchObject({
			outcome: 'fatal',
			error: 'never',
		});
	});

	it('reports a callback past its timeout as an error, and sends the timeout held to five minutes', async () => {
		const ledger = new FakeLedger();
		await turn(ledger, async (_, step) =>
			step.do(
				'slow',
				{ retries: { limit: 0, delay: 1 }, timeout: 20 },
				() => new Promise(() => {}),
			),
		);
		const [begun] = ledger.writtenTo('/steps/slow/attempts');
		expect(begun?.body.config).toMatchObject({ timeout_ms: 20 });
		expect(ledger.writtenTo('/steps/slow')[0]?.body).toMatchObject({
			outcome: 'error',
			error: 'the step timed out after 20 ms',
		});
		const defaults = new FakeLedger();
		await turn(defaults, async (_, step) => step.do('quick', async () => 1));
		expect(defaults.writtenTo('/steps/quick/attempts')[0]?.body.config).toMatchObject({
			timeout_ms: 300_000,
		});
	});

	it('gives the callback its attempt and its occurrence of the name', async () => {
		const ledger = new FakeLedger();
		const seen: unknown[] = [];
		await turn(ledger, async (_, step) => step.do('count', async (ctx) => seen.push(ctx)));
		expect(seen).toEqual([{ step: { name: 'count', count: 1 }, attempt: 1, config: {} }]);
	});

	it('returns a result as JSON reads it back, the same in the turn that ran it and the next', async () => {
		const ledger = new FakeLedger();
		const at = new Date('2026-10-14T00:00:00Z');
		const ended = await turn(ledger, async (_, step) =>
			step.do('when', async () => ({ at, nothing: undefined })),
		);
		expect(ended).toEqual({ ended: 'done', output: { at: '2026-10-14T00:00:00.000Z' } });
	});

	it('fails a step whose result is past 1 MiB', async () => {
		const ledger = new FakeLedger();
		const ended = await turn(ledger, async (_, step) =>
			step.do('huge', async () => 'x'.repeat(1 << 20)),
		);
		expect(ended).toEqual({ ended: 'failed', error: 'the result is larger than 1 MiB' });
	});

	it('fixes a sleep instant once, and never asks again once it is done', async () => {
		const ledger = new FakeLedger();
		const before = Date.now();
		await turn(ledger, napping);
		const wake = Date.parse(String(ledger.writtenTo('/sleeps')[0]?.body.wake_at));
		expect(wake - before).toBeGreaterThanOrEqual(3_600_000);
		expect(wake - before).toBeLessThan(3_660_000);
		ledger.due();
		await turn(ledger, napping, 'tyo', regrant(ledger));
		const asked = ledger.writtenTo('/sleeps').map((write) => write.body.name);
		expect(asked).toEqual(['nap', 'until']);
		expect(ledger.writtenTo('/sleeps')[1]?.body.wake_at).toBe('2026-10-14T13:00:00.000Z');
	});

	it('gives an event in Workflows shape, and waits holding nothing until one is sent', async () => {
		const ledger = new FakeLedger();
		expect(await turn(ledger, approval)).toEqual({ ended: 'halted', why: 'turn' });
		expect(ledger.writtenTo('/waits')[0]?.body).toMatchObject({
			type: 'approval',
			timeout_ms: 3_600_000,
		});
		ledger.send('approval', { by: 'canmi' });
		const ended = await turn(ledger, approval, 'tyo', regrant(ledger));
		expect(ended).toEqual({
			ended: 'done',
			output: { payload: { by: 'canmi' }, timestamp: '2026-10-14T12:01:00.500Z', type: 'approval' },
		});
		const replayed: unknown[] = [];
		await turn(
			ledger,
			async (_, step) => replayed.push(await step.waitForEvent('approved', { type: 'approval' })),
			'tyo',
			regrant(ledger),
		);
		expect(replayed[0]).toEqual({
			payload: { by: 'canmi' },
			timestamp: new Date('2026-10-14T12:01:00.500Z'),
			type: 'approval',
		});
	});

	it('throws a wait that timed out, as Workflows does', async () => {
		const ledger = new FakeLedger();
		const ended = await turn(ledger, async (_, step) =>
			step.waitForEvent('approved', { type: 'approval', timeout: 0 }),
		);
		expect(ended).toEqual({ ended: 'failed', error: 'no approval event came in time' });
	});

	it('ends a turn only once every step awaited together has its answer', async () => {
		const ledger = new FakeLedger();
		const ended = await turn(ledger, async (_, step) =>
			Promise.all([
				step.do('fails', async () => {
					throw new Error('again');
				}),
				step.do('slow', async () => {
					await new Promise((resolve) => setTimeout(resolve, 30));
					return 'late';
				}),
			]),
		);
		expect(ended).toEqual({ ended: 'halted', why: 'turn' });
		expect(
			ledger.writtenTo('/steps/slow'),
			'the slow step reported before the turn ended',
		).toHaveLength(1);
	});

	it('stops at a lease that is no longer its own, saying nothing of the run', async () => {
		const ledger = new FakeLedger();
		const ended = await turn(ledger, async (_, step) => step.do('a', async () => 1), 'sin');
		expect(ended).toEqual({ ended: 'halted', why: 'stale' });
		expect(ledger.outcome).toBeUndefined();
	});

	it('fails the run whose code disagrees with the record', async () => {
		const ledger = new FakeLedger();
		await turn(ledger, async (_, step) =>
			step.do('same', async () => {
				throw new Error('again');
			}),
		);
		const ended = await turn(
			ledger,
			async (_, step) => step.sleep('same', '1 hour'),
			'tyo',
			regrant(ledger),
		);
		expect(ended).toMatchObject({ ended: 'failed' });
		expect(ledger.outcome).toMatchObject({
			outcome: 'failed',
			error: expect.stringMatching(/^replay_diverged: /),
		});
	});

	it('leaves the run to the next turn when the ledger cannot be reached', async () => {
		const ledger = new FakeLedger();
		ledger.down = true;
		const ended = await turn(ledger, async (_, step) => step.do('a', async () => 1));
		expect(ended).toMatchObject({ ended: 'halted', why: 'unreachable' });
	});

	it('refuses what this runner does not keep: a rollback', async () => {
		const ledger = new FakeLedger();
		const ended = await turn(ledger, async (_, step) =>
			(step.do as (...args: unknown[]) => Promise<unknown>)('a', async () => 1, {
				rollback: async () => {},
			}),
		);
		expect(ended).toEqual({ ended: 'failed', error: 'a rollback is not supported by this runner' });
	});
});
