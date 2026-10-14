/**
 * What a workflow imports as `cloudflare:workers` and `cloudflare:workflows`, for where workerd
 * does not provide them: under Node, and in the tests, both specifiers resolve to this file. The
 * shapes are Workflows' own, as `@cloudflare/workers-types` declares them, less what the ledger
 * does not keep. See RFC 23wgz, "The second slice, the executor".
 */

export type WorkflowDurationLabel =
	| 'second'
	| 'minute'
	| 'hour'
	| 'day'
	| 'week'
	| 'month'
	| 'year';
export type WorkflowSleepDuration = `${number} ${WorkflowDurationLabel}${'s' | ''}` | number;
export type WorkflowDelayDuration = WorkflowSleepDuration;
export type WorkflowTimeoutDuration = WorkflowSleepDuration;
export type WorkflowBackoff = 'constant' | 'linear' | 'exponential';

/** A step's retries and timeout; a delay given as a function is refused, see `wire`. */
export type WorkflowStepConfig = {
	retries?: {
		limit: number;
		delay: WorkflowDelayDuration | number;
		backoff?: WorkflowBackoff;
	};
	timeout?: WorkflowTimeoutDuration | number;
	sensitive?: 'output';
};

export type WorkflowEvent<T> = {
	payload: Readonly<T>;
	timestamp: Date;
	instanceId: string;
	workflowName: string;
};

export type WorkflowStepEvent<T> = {
	payload: Readonly<T>;
	timestamp: Date;
	type: string;
};

export type WorkflowStepContext = {
	step: { name: string; count: number };
	attempt: number;
	config: WorkflowStepConfig;
};

export interface WorkflowStep {
	do<T>(name: string, callback: (ctx: WorkflowStepContext) => Promise<T>): Promise<T>;
	do<T>(
		name: string,
		config: WorkflowStepConfig,
		callback: (ctx: WorkflowStepContext) => Promise<T>,
	): Promise<T>;
	sleep(name: string, duration: WorkflowSleepDuration): Promise<void>;
	sleepUntil(name: string, timestamp: Date | number): Promise<void>;
	waitForEvent<T>(
		name: string,
		options: { type: string; timeout?: WorkflowTimeoutDuration | number },
	): Promise<WorkflowStepEvent<T>>;
}

/** The part of workerd's `ExecutionContext` a workflow may touch. */
export type ExecutionContext = {
	waitUntil(promise: Promise<unknown>): void;
	passThroughOnException(): void;
};

export abstract class WorkflowEntrypoint<Env = unknown, T = unknown> {
	protected ctx: ExecutionContext;
	protected env: Env;

	constructor(ctx: ExecutionContext, env: Env) {
		this.ctx = ctx;
		this.env = env;
	}

	abstract run(event: Readonly<WorkflowEvent<T>>, step: WorkflowStep): Promise<unknown>;
}

/** A step's failure that is not retried, and fails the step at once. */
export class NonRetryableError extends Error {
	constructor(message: string, name = 'NonRetryableError') {
		super(message);
		this.name = name;
	}
}
