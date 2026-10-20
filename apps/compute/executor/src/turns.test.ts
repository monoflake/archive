import type { Turn } from '@monoflake/replay';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { type HttpLedger, httpLedger } from '../test/ledger.ts';
import type { Workflow } from '../workflows/index.ts';
import { WORKFLOWS } from '../workflows/index.ts';
import { threads } from './turns.ts';

let ledger: HttpLedger;
beforeAll(async () => {
	ledger = await httpLedger();
});
afterAll(() => ledger.close());

function fixture(name: string): Workflow {
	return { module: new URL('../test/workflows.ts', import.meta.url).href, export: name };
}

function turn(run: string, workflow: string): Turn {
	return {
		service: 'test',
		run,
		workflow,
		params: { hello: 'node' },
		asked_at: '2026-10-14T12:00:00.000000001Z',
		fence: 1,
		expires_at: '2026-10-14T12:00:30Z',
		done: {},
		partial: false,
	};
}

function start(heapMb?: number) {
	return threads({
		node: '0000000000000001',
		ledger: `${ledger.base}/ledger`,
		bindings: { SERVICE: `${ledger.base}/service` },
		workflows: {
			...WORKFLOWS,
			counter: fixture('Counter'),
			hog: fixture('Hog'),
			hang: fixture('Hang'),
			bound: fixture('Bound'),
		},
		heapMb,
	});
}

function outcomeOf(run: string) {
	return ledger.written.find((write) => write.path === `/ledger/v1/runs/test/${run}/outcome`)?.body;
}

describe('a turn in a thread', () => {
	it('replays a workflow written in Workflows API and reports its end', async () => {
		const ended = await start()(turn('echo-1', 'echo')).ended;
		expect(ended).toEqual({
			ended: 'done',
			output: { echoed: { hello: 'node' }, at: '2026-10-14T12:00:00.000Z' },
		});
		expect(outcomeOf('echo-1')).toMatchObject({
			node: '0000000000000001',
			fence: 1,
			outcome: 'done',
		});
	});

	it('says when its steps are asking the ledger, for the stall clock', async () => {
		const running = start()(turn('echo-busy', 'echo'));
		const told: boolean[] = [];
		running.onBusy((busy) => told.push(busy));
		expect(await running.ended).toMatchObject({ ended: 'done' });
		expect(told).toEqual([true, false]);
	});

	it('never says busy of code that awaits outside a step', async () => {
		const running = start()(turn('hang-busy', 'hang'));
		const told: boolean[] = [];
		running.onBusy((busy) => told.push(busy));
		await new Promise((resolve) => setTimeout(resolve, 300));
		running.terminate();
		await running.ended;
		expect(told).toEqual([]);
	});

	it('starts every turn in a global scope of its own', async () => {
		const run = start();
		const first = await run(turn('count-1', 'counter')).ended;
		const second = await run(turn('count-2', 'counter')).ended;
		expect([first, second]).toEqual([
			{ ended: 'done', output: { counted: 1 } },
			{ ended: 'done', output: { counted: 1 } },
		]);
	});

	it('reaches a service through the binding it was given', async () => {
		const ended = await start()(turn('bound-1', 'bound')).ended;
		expect(ended).toEqual({
			ended: 'done',
			output: { status: 'success', data: { id: 'one' } },
		});
	});

	it('ends a turn past its heap, reporting nothing', async () => {
		const ended = await start(16)(turn('hog-1', 'hog')).ended;
		expect(ended).toMatchObject({ ended: 'halted', why: 'unreachable' });
		expect(outcomeOf('hog-1')).toBeUndefined();
	}, 20_000);

	it('stops a turn that never ends when it is terminated', async () => {
		const running = start()(turn('hang-1', 'hang'));
		setTimeout(() => running.terminate(), 200);
		expect(await running.ended).toMatchObject({ ended: 'halted', why: 'unreachable' });
		expect(outcomeOf('hang-1')).toBeUndefined();
	});

	it('leaves a run of a workflow it does not have to its lease', async () => {
		const ended = await start()(turn('nobody-1', 'nobody')).ended;
		expect(ended).toEqual({
			ended: 'halted',
			why: 'unreachable',
			detail: 'no workflow is named nobody',
		});
	});
});
