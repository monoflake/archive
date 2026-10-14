/**
 * A turn started in a thread of its own, with a heap of at most 128 MB, l4s6a's limit for an
 * isolate. See spec/architecture/executor.md, "A thread stands in for an isolate".
 */

import { Worker } from 'node:worker_threads';
import type { Ended, Turn } from '@monoflake/replay';
import type { Workflow } from '../workflows/index.ts';
import type { Start } from './driver.ts';
import { THREAD } from './files.ts';

/** A turn's heap, in megabytes. See RFC l4s6a. */
export const HEAP_MB = 128;

/** What a thread is handed: everything a turn needs, as plain data. */
export interface TurnData {
	readonly turn: Turn;
	readonly node: string;
	readonly ledger: string;
	readonly bindings: Readonly<Record<string, string>>;
	readonly workflow: Workflow;
}

export interface Threads {
	readonly node: string;
	/** The ledger's base on the private side. */
	readonly ledger: string;
	readonly bindings: Readonly<Record<string, string>>;
	readonly workflows: Readonly<Record<string, Workflow>>;
	readonly heapMb?: number;
}

/** Starts each turn in a thread of its own; one the thread does not answer for is a lost holder. */
export function threads(options: Threads): Start {
	return (turn) => {
		const workflow = options.workflows[turn.workflow];
		if (!workflow) {
			const detail = `no workflow is named ${turn.workflow}`;
			return {
				ended: Promise.resolve({ ended: 'halted', why: 'unreachable', detail }),
				terminate() {},
				onBusy() {},
			};
		}
		const data: TurnData = {
			turn,
			node: options.node,
			ledger: options.ledger,
			bindings: options.bindings,
			workflow,
		};
		const worker = new Worker(THREAD, {
			workerData: data,
			resourceLimits: { maxOldGenerationSizeMb: options.heapMb ?? HEAP_MB },
		});
		const listeners: ((busy: boolean) => void)[] = [];
		const ended = new Promise<Ended>((resolve) => {
			// A thread says when its steps are busy, `{busy}`, and once how the turn ended.
			worker.on('message', (message: Ended | { busy: boolean }) => {
				if ('busy' in message) {
					for (const listener of listeners) listener(message.busy);
					return;
				}
				resolve(message);
				void worker.terminate();
			});
			worker.once('error', (error: Error) => {
				resolve({
					ended: 'halted',
					why: 'unreachable',
					detail: `the thread failed: ${error.message}`,
				});
			});
			worker.once('exit', (code) => {
				resolve({ ended: 'halted', why: 'unreachable', detail: `the thread exited with ${code}` });
			});
		});
		return {
			ended,
			terminate: () => void worker.terminate(),
			onBusy: (listener) => void listeners.push(listener),
		};
	};
}
