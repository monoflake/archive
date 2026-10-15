/**
 * What control reads from and writes to its database: the newest change, every resource of a
 * kind, one by its id or its name, one write held to the revision read, and an import replacing
 * them all. See RFC o7eir.
 */
import type { Identity } from './specs.ts';

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

/** One resource as `/v1/registry` answers it; `name` is null for a node and an account. */
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

/** How many ids are drawn for one write before a collision is taken as a fault. */
const DRAWS = 4;

const COLUMNS = 'kind, id, name, revision, spec';
// NULLS LAST as Postgres orders a name by default, so both registries list in one order.
const ORDER = 'ORDER BY name NULLS LAST, id';
const LATEST = 'SELECT coalesce(max(id), 0) AS revision FROM changes';

function resource(row: Row): Resource {
	return { ...row, spec: JSON.parse(row.spec) };
}

/** Whether the database answers, for `/v1/health`. */
export async function answers(db: Database): Promise<void> {
	await db.prepare('SELECT 1').first();
}

/** The newest change control holds, alone: one row, what a list's `ETag` is compared to. */
export async function revised(db: Database): Promise<number> {
	const latest = await db.prepare(LATEST).first<{ revision: number }>();
	return latest?.revision ?? 0;
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
 * Every resource replaced by `rows`, and every account's identities by `identities`, and a change
 * written at `revision` so the next one is numbered after it; nothing is written if control
 * already holds a newer change. Answers the newest change once the batch is done, which is
 * `revision` exactly when the import took.
 */
export async function replaced(
	db: Database,
	rows: readonly Row[],
	identities: readonly Owned[],
	revision: number,
	at: string,
): Promise<number> {
	const current = `(${LATEST}) <= ?1`;
	const [, , , , , latest] = await db.batch<{ revision: number }>([
		db.prepare(`DELETE FROM resources WHERE ${current}`).bind(revision),
		db.prepare(`DELETE FROM identities WHERE ${current}`).bind(revision),
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
				`INSERT INTO identities (provider, kind, value, account)
				SELECT json_extract(value, '$.provider'), json_extract(value, '$.kind'),
					json_extract(value, '$.value'), json_extract(value, '$.account')
				FROM json_each(?2) WHERE ${current}`,
			)
			.bind(revision, JSON.stringify(identities)),
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

/** An identity and the account holding it. */
export interface Owned extends Identity {
	account: string;
}

/** One write, held to the revision it was read at. */
export interface Change {
	kind: Kind;
	/** The resource's id: the one held, or one drawn for a resource made now. */
	id: string;
	/** An app's name, none for a node and an account. */
	name: string | null;
	/** Whether it is made now, by a `POST` or by an app's name no app holds. */
	made: boolean;
	/** The revision it was read at, 0 for a resource made now. */
	revision: number;
	/** Its spec, canonical. */
	spec: string;
	by: string;
	at: string;
	/** An account's identities as compared, which replace the ones it held; none for another kind. */
	identities?: readonly Identity[];
}

/** A write that landed: the resource's id, its revision from now on, and the change's task. */
export interface Landed {
	id: string;
	revision: number;
	task: string;
}

/** 16 lowercase hexadecimal digits, drawn from the platform's random source. */
export function drawn(): string {
	const bytes = crypto.getRandomValues(new Uint8Array(8));
	return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}

/**
 * `change` written, its task and, for a resource made now, its id drawn again past a collision at
 * most `DRAWS` times; none when it was stale. A clash of identities is thrown as it is.
 */
export async function committed(
	db: Database,
	change: Omit<Change, 'id'> & { id?: string },
): Promise<Landed | undefined> {
	for (let draw = 0; draw < DRAWS; draw++) {
		const id = change.id ?? drawn();
		const task = drawn();
		try {
			// oxlint-disable-next-line no-await-in-loop -- a draw again only after a collision
			const revision = await set(db, { ...change, id }, task);
			return revision === undefined ? undefined : { id, revision, task };
		} catch (error) {
			if (!/UNIQUE constraint failed: (resources\.id|changes\.task)/.test(String(error))) {
				throw error;
			}
		}
	}
	throw new Error(`${DRAWS} ids drawn for one write were each taken already`);
}

/** Whether `error` is a write refused for an identity another account holds. */
export function clashed(error: unknown): boolean {
	return String(error).includes('UNIQUE constraint failed: identities.');
}

/**
 * One batch, so a write and its change land together or not at all: the change is appended only
 * while the resource is still at the revision read, or, made now, while no app holds its name;
 * the resource takes the change's id as its revision. Answers that revision, or none when stale.
 * An id already held fails the batch, as a task does.
 */
export async function set(db: Database, change: Change, task: string): Promise<number | undefined> {
	const { kind, id, name, made, revision, spec, by, at } = change;
	const appended = made
		? db
				.prepare(
					`INSERT INTO changes (at, by, kind, resource, name, task, before, after)
						SELECT ?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7
						WHERE NOT EXISTS (SELECT 1 FROM resources WHERE kind = ?3 AND name = ?5)
						RETURNING id`,
				)
				.bind(at, by, kind, id, name, task, spec)
		: db
				.prepare(
					`INSERT INTO changes (at, by, kind, resource, name, task, before, after)
						SELECT ?1, ?2, kind, id, name, ?6, spec, ?7 FROM resources
						WHERE kind = ?3 AND id = ?4 AND revision = ?5
						RETURNING id`,
				)
				.bind(at, by, kind, id, revision, task, spec);
	const stored = made
		? db
				.prepare(
					`INSERT INTO resources (kind, id, name, revision, spec, updated_at)
						SELECT ?1, ?2, ?3, id, ?4, ?5 FROM changes WHERE task = ?6`,
				)
				.bind(kind, id, name, spec, at, task)
		: db
				.prepare(
					`UPDATE resources SET spec = ?1, updated_at = ?2,
							revision = (SELECT id FROM changes WHERE task = ?3)
						WHERE id = ?4 AND revision = ?5`,
				)
				.bind(spec, at, task, id, revision);
	const statements = [appended, stored];
	if (change.identities !== undefined) {
		const landed = 'EXISTS (SELECT 1 FROM changes WHERE task = ?2)';
		statements.push(
			db.prepare(`DELETE FROM identities WHERE account = ?1 AND ${landed}`).bind(id, task),
			db
				.prepare(
					`INSERT INTO identities (provider, kind, value, account)
					SELECT json_extract(value, '$.provider'), json_extract(value, '$.kind'),
						json_extract(value, '$.value'), ?1
					FROM json_each(?3) WHERE ${landed}`,
				)
				.bind(id, task, JSON.stringify(change.identities)),
		);
	}
	const [appendedRows] = await db.batch<{ id: number }>(statements);
	return appendedRows?.results[0]?.id;
}

/** Which of `ids` name no node control holds, or one retired, in the order given. */
export async function unplaceable(db: Database, ids: readonly string[]): Promise<string[]> {
	const { results } = await db
		.prepare(
			`SELECT id FROM resources WHERE kind = 'node'
			AND id IN (SELECT value FROM json_each(?1))
			AND coalesce(json_extract(spec, '$.retired'), 0) = 0`,
		)
		.bind(JSON.stringify(ids))
		.all<{ id: string }>();
	const placeable = new Set(results.map((row) => row.id));
	return ids.filter((id) => !placeable.has(id));
}

/** The account other than `own` holding one of `identities`, and that identity's kind. */
export async function clashing(
	db: Database,
	identities: readonly Identity[],
	own: string | null,
): Promise<{ account: string; kind: string } | undefined> {
	const row = await db
		.prepare(
			`SELECT held.account, held.kind FROM identities held, json_each(?1) given
			WHERE held.provider = json_extract(given.value, '$.provider')
			AND held.kind = json_extract(given.value, '$.kind')
			AND held.value = json_extract(given.value, '$.value')
			AND held.account IS NOT ?2
			LIMIT 1`,
		)
		.bind(JSON.stringify(identities), own)
		.first<{ account: string; kind: string }>();
	return row ?? undefined;
}
