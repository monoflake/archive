/**
 * What the executor is told, read from its environment: where it listens, which node it is, how
 * many leases it holds at once, where the ledger is and the bindings a workflow is given. See
 * spec/architecture/executor.md, "Configuration".
 */

import { URLS } from '@monoflake/sdk';

/** A ten past the deployer's port: spec/architecture/services.md, "A service keeps one port". */
export const PORT = 12030;

/** How many leases it holds at once, turns and placed steps together, unless told. */
export const CAPACITY = 4;

export type Env = Readonly<Record<string, string | undefined>>;

export interface Config {
	/** Where its doorbell and health answer: `LISTEN`, `host:port`, as every service reads it. */
	readonly listen: { readonly host: string; readonly port: number };
	/**
	 * The node it runs on, by its id: `NODE_ID`, which host sets on every container beside `NODE`,
	 * the hostname. Named in every write, and what the ledger pushes a turn by.
	 */
	readonly node: string | undefined;
	readonly capacity: number;
	/** The ledger on the private side: `LEDGER_URL`, the URL map's when unset. */
	readonly ledger: string;
	/** Where the ledger rings it: `PUSH_URL`, its own scope on the private side when unset. */
	readonly pushUrl: string;
	/** Each binding a workflow is given and the base it reaches: the URL map's, and `BINDINGS`. */
	readonly bindings: Readonly<Record<string, string>>;
}

/**
 * The bindings every workflow is given unless `BINDINGS` says otherwise: the platform's services
 * on the private side, by their scope.
 */
export function defaultBindings(): Record<string, string> {
	return { LEDGER: URLS.internal.ledger };
}

export function configOf(env: Env): Config {
	const bindings = {
		...defaultBindings(),
		...Object.fromEntries(pairs(value(env, 'BINDINGS') ?? '')),
	};
	return {
		listen: listenOf(value(env, 'LISTEN')),
		node: value(env, 'NODE_ID'),
		capacity: capacityOf(value(env, 'CAPACITY')),
		ledger: value(env, 'LEDGER_URL') ?? URLS.internal.ledger,
		pushUrl: value(env, 'PUSH_URL') ?? `${URLS.internal.api.private}/executor/turns`,
		bindings,
	};
}

/** `host:port`, the host a bracketed IPv6 address or a name; `0.0.0.0:12030` when unset. */
export function listenOf(text: string | undefined): Config['listen'] {
	const at = /^\[?([^\]]*)\]?:(\d+)$/.exec(text ?? '');
	if (!at) return { host: '0.0.0.0', port: PORT };
	return { host: at[1] || '0.0.0.0', port: Number(at[2]) };
}

/** A whole number of leases, at least one; anything else is the default. */
export function capacityOf(text: string | undefined): number {
	const capacity = Number(text);
	return Number.isInteger(capacity) && capacity >= 1 ? capacity : CAPACITY;
}

/** `NAME=url` pairs separated by commas, each side trimmed; a pair missing either is dropped. */
export function pairs(text: string): [string, string][] {
	return text.split(',').flatMap((pair) => {
		const at = pair.indexOf('=');
		const key = pair.slice(0, at).trim();
		const rest = pair.slice(at + 1).trim();
		return at > 0 && key && rest ? [[key, rest] as [string, string]] : [];
	});
}

function value(env: Env, key: string): string | undefined {
	const found = env[key]?.trim();
	return found ? found : undefined;
}
