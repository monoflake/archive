/**
 * The world an end-to-end test runs in: a database of its own in the Postgres
 * `LEDGER_TEST_DATABASE_URL` names, the ledger at `LEDGER_BIN` running against it on a port of its
 * own, a Patroni that always says this node leads, and a witness the workflows report to. Time is
 * moved by SQL, never waited for; every wait has a deadline. See
 * apps/data/ledger/src/store/testing.rs, which the SQL tests follow.
 */

import { type ChildProcess, spawn } from 'node:child_process';
import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import postgres from 'postgres';

/** The node the ledger runs on, so a grant to an executor named so is pushed. */
export const LEDGER_NODE = '0000000000000001';

/** Whether this run has what the tests need; said once, and refused in CI. */
export function ready(): boolean {
	const missing = ['LEDGER_TEST_DATABASE_URL', 'LEDGER_BIN'].filter((key) => !process.env[key]);
	if (missing.length === 0) return true;
	const unset = `${missing.join(' and ')} ${missing.length > 1 ? 'are' : 'is'} not set`;
	if (process.env.CI) throw new Error(`CI is set and ${unset}: CI runs every e2e test`);
	console.log(`${unset}; the executor's e2e tests are not run`);
	return false;
}

/** Answers once `check` says so, asking every 50 ms; fails naming `what` past `ms`. */
export function until(
	what: string,
	check: () => Promise<boolean> | boolean,
	ms = 10_000,
): Promise<void> {
	const deadline = Date.now() + ms;
	const attempt = async (): Promise<void> => {
		if (await check()) return;
		if (Date.now() > deadline) throw new Error(`${what} did not happen within ${ms} ms`);
		await new Promise((resolve) => setTimeout(resolve, 50));
		return attempt();
	};
	return attempt();
}

async function listening(server: Server): Promise<number> {
	await new Promise<void>((resolve) => server.listen(0, resolve));
	return (server.address() as AddressInfo).port;
}

/** A port nobody holds at the moment of asking. */
async function freePort(): Promise<number> {
	const server = createServer();
	const port = await listening(server);
	await new Promise((resolve) => server.close(resolve));
	return port;
}

/**
 * What workflows report to through their `WITNESS` binding: each path asked, counted, and a path
 * under a held prefix kept waiting until it is let go.
 */
export class Witness {
	readonly asked: string[] = [];
	readonly #held = new Map<string, (() => void)[]>();
	readonly #server = createServer((request, response) => {
		const path = request.url ?? '';
		this.asked.push(path);
		const answer = () => response.end('{}');
		const prefix = [...this.#held.keys()].find((held) => path.startsWith(held));
		if (prefix) this.#held.get(prefix)?.push(answer);
		else answer();
	});
	base = '';

	async start(): Promise<void> {
		this.base = `http://localhost:${await listening(this.#server)}`;
	}

	count(path: string): number {
		return this.asked.filter((asked) => asked === path).length;
	}

	hold(prefix: string): void {
		this.#held.set(prefix, this.#held.get(prefix) ?? []);
	}

	release(prefix: string): void {
		for (const answer of this.#held.get(prefix) ?? []) answer();
		this.#held.delete(prefix);
	}

	close(): Promise<void> {
		for (const prefix of this.#held.keys()) this.release(prefix);
		this.#server.closeAllConnections();
		return new Promise((resolve) => this.#server.close(() => resolve()));
	}
}

export interface RunView {
	run: { status: string; fence: number; output: unknown; error: string | null };
	steps: {
		name: string;
		status: string;
		attempt: number;
		starts: number;
		wake_at: string | null;
	}[];
	lease: { holder: string; fence: number; expires_at: string } | null;
}

export class World {
	readonly witness = new Witness();
	ledger = '';
	sql!: postgres.Sql;
	#admin!: postgres.Sql;
	#database = '';
	#process: ChildProcess | undefined;
	#patroni = createServer((_, response) => response.end('leader'));
	#log: string[] = [];

	async start(): Promise<void> {
		const url = process.env.LEDGER_TEST_DATABASE_URL as string;
		this.#database = `e2e_${process.pid}_${Date.now()}`;
		this.#admin = postgres(url, { max: 1, onnotice: () => {} });
		await this.#admin.unsafe(`CREATE DATABASE ${this.#database}`);
		const own = new URL(url);
		own.pathname = `/${this.#database}`;
		this.sql = postgres(own.href, { max: 2, onnotice: () => {} });
		const patroni = await listening(this.#patroni);
		const port = await freePort();
		this.ledger = `http://localhost:${port}`;
		this.#process = spawn(process.env.LEDGER_BIN as string, [], {
			env: {
				...process.env,
				DATABASE_URL: own.href,
				PATRONI_PRIMARY_URL: `http://localhost:${patroni}/primary`,
				LISTEN: `127.0.0.1:${port}`,
				NODE_ID: LEDGER_NODE,
			},
			stdio: ['ignore', 'pipe', 'pipe'],
		});
		const keep = (chunk: Buffer) => this.#log.push(chunk.toString());
		this.#process.stdout?.on('data', keep);
		this.#process.stderr?.on('data', keep);
		await this.witness.start();
		await until(
			'the ledger answering',
			async () => (await fetch(`${this.ledger}/health`).catch(() => undefined))?.ok === true,
			20_000,
		).catch((error: Error) => {
			throw new Error(`${error.message}; it said: ${this.#log.join('')}`);
		});
	}

	async stop(): Promise<void> {
		this.#process?.kill('SIGTERM');
		await this.witness.close();
		await new Promise((resolve) => this.#patroni.close(resolve));
		await this.sql?.end();
		await this.#admin?.unsafe(`DROP DATABASE IF EXISTS ${this.#database} WITH (FORCE)`);
		await this.#admin?.end();
	}

	async call(method: string, path: string, body?: unknown): Promise<{ status: number; data: any }> {
		const response = await fetch(`${this.ledger}${path}`, {
			method,
			headers: { 'content-type': 'application/json' },
			body: body === undefined ? undefined : JSON.stringify(body),
		});
		const envelope = (await response.json()) as { data?: unknown; code?: string };
		return { status: response.status, data: envelope.data ?? envelope };
	}

	async open(id: string, workflow: string, params: unknown = {}): Promise<void> {
		const { status, data } = await this.call('PUT', `/v1/runs/e2e/${id}`, { workflow, params });
		if (status !== 201)
			throw new Error(`opening ${id} answered ${status}: ${JSON.stringify(data)}`);
	}

	async view(id: string): Promise<RunView> {
		return (await this.call('GET', `/v1/runs/e2e/${id}`)).data as RunView;
	}

	async status(id: string): Promise<string> {
		return (await this.view(id)).run.status;
	}

	/** Every sleep, wait and retry due now, as the ledger's own tests make them. */
	async due(): Promise<void> {
		await this.sql`UPDATE runs SET run_at = 0 WHERE status IN ('sleeping', 'waiting')`;
		await this.sql`UPDATE steps SET wake_at = 0 WHERE status IN ('sleeping', 'waiting')`;
	}

	/** Every lease run out now. */
	async lapse(): Promise<void> {
		await this.sql`UPDATE leases SET expires_at = 0`;
	}
}
