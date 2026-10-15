/**
 * D1 for the tests: node's SQLite behind the part of D1's surface store.ts uses, a batch one
 * transaction as D1's is, with the real migrations applied in order.
 */
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { DatabaseSync, type SQLInputValue } from 'node:sqlite';
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
