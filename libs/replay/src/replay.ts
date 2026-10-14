/**
 * One turn: the workflow run from the top against what the ledger kept, until it returns, throws,
 * or a step ends the turn; how it ended is told to the ledger. Web APIs alone, so the same code
 * runs in a worker thread now and in a workerd isolate later. See RFC 23wgz.
 */

import type { ExecutionContext, WorkflowEntrypoint, WorkflowEvent } from 'cloudflare:workers';
import { type LedgerFetch, RunClient, Unreachable, instant } from './ledger.ts';
import { type Halt, Steps } from './step.ts';

/** A turn as the ledger hands it out. See spec/architecture/ledger.md, "How a node gets a turn". */
export type Turn = {
	service: string;
	run: string;
	workflow: string;
	params: unknown;
	asked_at: string;
	fence: number;
	expires_at: string;
	done: Record<string, unknown>;
	partial: boolean;
};

/** How a turn ended: the run's end, told to the ledger, or a halt leaving the run to the next. */
export type Ended =
	| { ended: 'done'; output: unknown }
	| { ended: 'failed'; error: string }
	| { ended: 'halted'; why: 'turn' | 'stale' | 'unreachable'; detail?: string };

export type Entrypoint<Env> = new (ctx: ExecutionContext, env: Env) => WorkflowEntrypoint<Env>;

export type Replay<Env> = {
	turn: Turn;
	/** The node holding the turn's lease, named in every write. */
	node: string;
	entrypoint: Entrypoint<Env>;
	env: Env;
	ledger: LedgerFetch;
};

type Settled =
	| { settled: 'returned'; output: unknown }
	| { settled: 'threw'; error: unknown }
	| { settled: 'halted' };

export async function replay<Env>({
	turn,
	node,
	entrypoint,
	env,
	ledger,
}: Replay<Env>): Promise<Ended> {
	const client = new RunClient(ledger, turn.service, turn.run, node, turn.fence);
	const steps = new Steps(client, turn.done);
	let event: WorkflowEvent<unknown>;
	try {
		event = {
			payload: turn.params as Readonly<unknown>,
			timestamp: instant(turn.asked_at),
			instanceId: turn.run,
			workflowName: turn.workflow,
		};
	} catch (error) {
		return { ended: 'halted', why: 'unreachable', detail: String(error) };
	}
	const ctx: ExecutionContext = {
		waitUntil: (promise) => void promise.catch(() => {}),
		passThroughOnException: () => {},
	};
	const ran: Promise<Settled> = (async () => new entrypoint(ctx, env).run(event, steps))().then(
		(output) => ({ settled: 'returned', output }),
		(error: unknown) => ({ settled: 'threw', error }),
	);
	const settled = await Promise.race([
		ran,
		steps.idle.then((): Settled => ({ settled: 'halted' })),
	]);
	// A run that threw because a step it awaited with others failed while another ended the turn
	// is replayed next turn, where it throws again or goes on.
	if (settled.settled === 'halted' || steps.halted) return halted(client, await steps.idle);
	if (settled.settled === 'threw')
		return end(client, { ended: 'failed', error: message(settled.error) });
	let output: unknown;
	try {
		output = settled.output === undefined ? null : JSON.parse(JSON.stringify(settled.output));
	} catch (error) {
		return end(client, { ended: 'failed', error: `the output is not JSON: ${message(error)}` });
	}
	return end(client, { ended: 'done', output });
}

/** A halt, and the one kind of it that ends the run: code that disagrees with the record. */
function halted(client: RunClient, halt: Halt): Promise<Ended> | Ended {
	if (halt.why === 'diverged') {
		return end(client, { ended: 'failed', error: `replay_diverged: ${halt.detail}` });
	}
	return { ended: 'halted', ...halt };
}

/** The run's end, told to the ledger under the turn's lease. */
async function end(client: RunClient, ended: Ended & { ended: 'done' | 'failed' }): Promise<Ended> {
	const body =
		ended.ended === 'done'
			? { outcome: 'done', output: ended.output }
			: { outcome: 'failed', error: ended.error };
	try {
		const answer = await client.call('PUT', '/outcome', body);
		if (answer.ok) return ended;
		if (answer.code === 'stale_lease') return { ended: 'halted', why: 'stale' };
		return { ended: 'halted', why: 'unreachable', detail: `${answer.status} ${answer.code}` };
	} catch (error) {
		if (error instanceof Unreachable)
			return { ended: 'halted', why: 'unreachable', detail: error.message };
		throw error;
	}
}

function message(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
