/**
 * D1 for the tests: node's SQLite behind the part of D1's surface store.ts uses, a batch one
 * transaction as D1's is, with the real migrations applied in order. And control as a test calls
 * it, over one, holding the old registry as an import carries it.
 */
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { DatabaseSync, type SQLInputValue } from 'node:sqlite';
import type { ExecutionContext } from 'hono';
import { expect } from 'vitest';
import { app, type Env } from './index.ts';
import type { Database, Statement } from './store.ts';

const MIGRATIONS = join(import.meta.dirname, '../migrations');

class Prepared implements Statement {
	constructor(
		private readonly db: DatabaseSync,
		private readonly query: string,
		private readonly values: SQLInputValue[] = [],
	) {}

	bind(...values: unknown[]): Statement {
		return new Prepared(this.db, this.query, values.map(input));
	}

	rows(): unknown[] {
		return this.db.prepare(this.query).all(...this.values);
	}

	async first<T>(): Promise<T | null> {
		const [row] = this.rows();
		return (row as T | undefined) ?? null;
	}

	async all<T>(): Promise<{ results: T[] }> {
		return { results: this.rows() as T[] };
	}
}

/** What D1 binds, and nothing it does not: a value of any other type is a test's mistake. */
function input(value: unknown): SQLInputValue {
	if (value === null || typeof value === 'string' || typeof value === 'number') return value;
	throw new TypeError(`D1 binds no ${typeof value}`);
}

export class Sqlite implements Database {
	readonly db = new DatabaseSync(':memory:');

	constructor() {
		const files = readdirSync(MIGRATIONS).filter((name) => name.endsWith('.sql'));
		for (const file of files.toSorted()) {
			this.db.exec(readFileSync(join(MIGRATIONS, file), 'utf8'));
		}
	}

	prepare(query: string): Statement {
		return new Prepared(this.db, query);
	}

	async batch<T>(statements: Statement[]): Promise<{ results: T[] }[]> {
		this.db.exec('BEGIN');
		try {
			const results = statements.map((statement) => {
				if (!(statement instanceof Prepared)) throw new TypeError('not one of these statements');
				return { results: statement.rows() as T[] };
			});
			this.db.exec('COMMIT');
			return results;
		} catch (error) {
			this.db.exec('ROLLBACK');
			throw error;
		}
	}
}

// A reserved name, which never resolves.
export const ORIGIN = 'https://control.test';
export const READ = 'read-token';
export const WRITE = 'write-token';
export const IMPORT = 'import-token';

export const TYO = '00000000000000a1';
export const SIN = '00000000000000b2';
export const WEB = '0000000000000c01';
export const GEO = '0000000000000c02';
export const CDN = '0000000000000c03';
export const ORACLE = '0000000000000d01';

/** A node's record as control stores it. */
export function shown(address: string, city: string, country: string) {
	return {
		address,
		tier: 'datacenter',
		system: 'debian',
		city,
		country,
		continent: 'AS',
		roles: ['compute', 'core'],
	};
}

/** The old registry as an import carries it, its newest change at 41. */
export function body() {
	return {
		revision: 41,
		nodes: [
			{ id: SIN, name: null, revision: 12, spec: shown('100.64.0.2', 'Singapore', 'SG') },
			{ id: TYO, name: null, revision: 11, spec: shown('100.64.0.1', 'Tokyo', 'JP') },
		],
		apps: [
			{ id: WEB, name: 'web', revision: 30, spec: { placements: [TYO] } },
			{ id: GEO, name: 'geo', revision: 40, spec: { placements: [TYO, SIN] } },
			{ id: CDN, name: 'cdn', revision: 41, spec: { placements: [], display_name: 'CDN' } },
		],
		accounts: [
			{
				id: ORACLE,
				name: null,
				revision: 5,
				spec: { provider: 'oracle', identities: [{ kind: 'email', value: 'a@example.com' }] },
			},
		],
	};
}

/** What control answers a call, the envelope read. */
export interface Answer {
	status: number;
	body: Record<string, unknown>;
}

/** A Worker's execution context, keeping what each answer gave `waitUntil` for a test to await. */
export function context() {
	const waited: Promise<unknown>[] = [];
	const ctx: ExecutionContext = {
		waitUntil: (promise) => void waited.push(promise),
		passThroughOnException: () => {},
		props: {},
	};
	return { ctx, waited };
}

export function control(
	db: Database = new Sqlite(),
	tokens: Partial<Env> = {},
	{ ctx } = context(),
) {
	const env: Env = {
		DB: db,
		CONTROL_READ_TOKEN: READ,
		CONTROL_WRITE_TOKEN: WRITE,
		CONTROL_IMPORT_TOKEN: IMPORT,
		...tokens,
	};
	return async (
		method: string,
		path: string,
		options: { token?: string; body?: unknown } = {},
	): Promise<Answer> => {
		const headers = new Headers();
		if (options.token !== undefined) headers.set('Authorization', `Bearer ${options.token}`);
		const sent = options.body === undefined ? undefined : JSON.stringify(options.body);
		const response = await app.fetch(
			new Request(new URL(path, ORIGIN), { method, headers, body: sent }),
			env,
			ctx,
		);
		return { status: response.status, body: (await response.json()) as Record<string, unknown> };
	};
}

/** control holding `body()`, imported. */
export async function imported(db = new Sqlite()) {
	const call = control(db);
	expect(await call('PUT', '/v1/registry/import', { token: IMPORT, body: body() })).toEqual({
		status: 200,
		body: { status: 'success', data: { revision: 41 } },
	});
	return { db, call };
}

export function refused(status: number, code: string) {
	return { status, body: { status: 'error', code, message: expect.any(String) } };
}
