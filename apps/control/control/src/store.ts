/**
 * What control reads from and writes to its database: every resource of a kind, one by its id or
 * its name, and an import replacing them all. See RFC o7eir.
 */

/** The part of a D1 statement used here, as structure, so the tests can stand SQLite in for it. */
export interface Statement {
	bind(...values: unknown[]): Statement;
	first<T = Record<string, unknown>>(): Promise<T | null>;
	all<T = Record<string, unknown>>(): Promise<{ results: T[] }>;
}

/** The part of a D1 database used here. A batch runs as one transaction. */
export interface Database {
	prepare(query: string): Statement;
	batch<T = unknown>(statements: Statement[]): Promise<{ results: T[] }[]>;
}

export type Kind = 'node' | 'app' | 'account';

/** One resource as `/v1` answers it; `name` is null for a node and an account. */
export interface Resource {
	kind: Kind;
	id: string;
	name: string | null;
	revision: number;
	spec: unknown;
}

/** Every resource asked for, and the newest change control holds, read in one transaction. */
export interface Listed {
	items: Resource[];
	revision: number;
}

/** A resource as an import writes it, its spec already canonical. */
export interface Row {
	kind: Kind;
	id: string;
	name: string | null;
	revision: number;
	spec: string;
}

/** What narrows a list: an app's name, or the node an app is placed on. */
export interface Filter {
	name?: string;
	node?: string;
}

const COLUMNS = 'kind, id, name, revision, spec';
// NULLS LAST as Postgres orders a name by default, so both registries list in one order.
const ORDER = 'ORDER BY name NULLS LAST, id';
const LATEST = 'SELECT coalesce(max(id), 0) AS revision FROM changes';

function resource(row: Row): Resource {
	return { ...row, spec: JSON.parse(row.spec) };
}

/** Whether the database answers, for `/health`. */
export async function answers(db: Database): Promise<void> {
	await db.prepare('SELECT 1').first();
}

export async function listed(db: Database, kind: Kind, filter: Filter): Promise<Listed> {
	const values: unknown[] = [kind];
	const where = ['kind = ?1'];
	if (filter.name !== undefined) {
		values.push(filter.name);
		where.push(`name = ?${values.length}`);
	}
	if (filter.node !== undefined) {
		values.push(filter.node);
		where.push(
			`EXISTS (SELECT 1 FROM json_each(spec, '$.placements') WHERE value = ?${values.length})`,
		);
	}
	const items = `SELECT ${COLUMNS} FROM resources WHERE ${where.join(' AND ')} ${ORDER}`;
	const [rows, latest] = await db.batch<Row>([
		db.prepare(items).bind(...values),
		db.prepare(LATEST),
	]);
	return {
		items: (rows?.results ?? []).map(resource),
		revision: latest?.results[0]?.revision ?? 0,
	};
}

/** The resource of `kind` with this id, or the app with this name. */
export async function found(
	db: Database,
	kind: Kind,
	key: { id: string } | { name: string },
): Promise<Resource | undefined> {
	const column = 'id' in key ? 'id' : 'name';
	const value = 'id' in key ? key.id : key.name;
	const row = await db
		.prepare(`SELECT ${COLUMNS} FROM resources WHERE kind = ?1 AND ${column} = ?2`)
		.bind(kind, value)
		.first<Row>();
	return row ? resource(row) : undefined;
}

/**
 * Every resource replaced by `rows`, and a change written at `revision` so the next one is
 * numbered after it; nothing is written if control already holds a newer change. Answers the
 * newest change once the batch is done, which is `revision` exactly when the import took.
 */
export async function replaced(
	db: Database,
	rows: readonly Row[],
	revision: number,
	at: string,
): Promise<number> {
	const current = `(${LATEST}) <= ?1`;
	const [, , , latest] = await db.batch<{ revision: number }>([
		db.prepare(`DELETE FROM resources WHERE ${current}`).bind(revision),
		db
			.prepare(
				`INSERT INTO resources (kind, id, name, revision, spec, updated_at)
				SELECT json_extract(value, '$.kind'), json_extract(value, '$.id'),
					json_extract(value, '$.name'), json_extract(value, '$.revision'),
					json_extract(value, '$.spec'), ?3
				FROM json_each(?2) WHERE ${current}`,
			)
			.bind(revision, JSON.stringify(rows), at),
		db
			.prepare(
				`INSERT INTO changes (id, at, by, kind) SELECT ?1, ?2, 'import', 'import'
				WHERE ?1 > 0 AND ${current} ON CONFLICT (id) DO NOTHING`,
			)
			.bind(revision, at),
		db.prepare(LATEST),
	]);
	return latest?.results[0]?.revision ?? 0;
}
