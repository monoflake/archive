/**
 * A node as the end-to-end tests drive it: polled by hand, so a test chooses when a turn is taken,
 * and each turn run in a thread as the driver runs it.
 */

import type { Turn } from '@monoflake/replay';
import { threads } from '../../src/turns.ts';
import type { Workflow } from '../../workflows/index.ts';
import { until, type World } from './world.ts';

function at(name: string): Workflow {
	return { module: new URL('./workflows.ts', import.meta.url).href, export: name };
}

/** Every workflow the tests open runs of, by the name a run is opened with. */
export const WORKFLOWS: Readonly<Record<string, Workflow>> = {
	issue: at('Issue'),
	hold: at('Hold'),
	flaky: at('Flaky'),
	fatal: at('Fatal'),
	huge: at('Huge'),
	before: at('Before'),
	after: at('After'),
	approve: at('Approve'),
};

export class Node {
	readonly start: ReturnType<typeof threads>;
	readonly name: string;
	readonly workflows: readonly string[];
	readonly #world: World;

	constructor(
		world: World,
		name: string,
		workflows: readonly string[] = Object.keys(WORKFLOWS),
		witness = world.witness.base,
	) {
		this.#world = world;
		this.name = name;
		this.workflows = workflows;
		this.start = threads({
			node: name,
			ledger: world.ledger,
			bindings: { WITNESS: witness },
			workflows: WORKFLOWS,
		});
	}

	/** The heartbeat, and the turns this node holds now. */
	async poll(capacity = 4): Promise<Turn[]> {
		const body = { capacity, workflows: this.workflows };
		const { status, data } = await this.#world.call(
			'POST',
			`/v1/executors/${this.name}/polls`,
			body,
		);
		if (status !== 200) throw new Error(`a poll answered ${status}: ${JSON.stringify(data)}`);
		return data.turns as Turn[];
	}

	/** The turn of `run` the ledger grants this node next, above `fence`, polling until it does. */
	async granted(run: string, fence = 0): Promise<Turn> {
		let found: Turn | undefined;
		await until(`a turn of ${run} above fence ${fence} for ${this.name}`, async () => {
			found = (await this.poll()).find((turn) => turn.run === run && turn.fence > fence);
			return found !== undefined;
		});
		return found as Turn;
	}

	/** The next turn of `run` replayed in a thread, and how it ended. */
	async turn(run: string, fence = 0) {
		const turn = await this.granted(run, fence);
		return { turn, ended: await this.start(turn).ended };
	}
}
