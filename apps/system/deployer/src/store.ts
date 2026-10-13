/**
 * Every deploy, one row each, moving through its stages as it happens, as host keeps its history:
 * a finished row keeps where it failed, and nothing is pruned. See spec/architecture/deployer.md,
 * "What it refuses", and infra's spec/architecture/host.md, "What host keeps, and where".
 */
import { DatabaseSync } from 'node:sqlite';

export type Stage =
	| 'downloading'
	| 'admitting'
	| 'uploading'
	| 'deployed'
	| 'failed'
	// Left alone on purpose, as host's skipped rows are: nothing went wrong, and `error` says why.
	| 'skipped';

/** A deploy from a run, or a rollback the operator asked for, which has no run. */
export type Action = 'deploy' | 'rollback';

/** A row as `/api/deploys` answers it. */
export interface Deploy {
	readonly id: number;
	readonly action: Action;
	/** Absent on a run GitHub never gave, whose Workers were never known. */
	readonly worker?: string;
	readonly repository: string;
	readonly run: number | null;
	readonly commit: string | null;
	/** Run with `--dry-run`: recorded, and nothing deployed. */
	readonly dry: boolean;
	readonly stage: Stage;
	/** The stage a failed row was in when it failed. */
	readonly failed_in: Stage | null;
	/** The version Cloudflare returned, once deployed; for a rollback, the version asked for. */
	readonly version: string | null;
	/** Why it failed, or why it was skipped. */
	readonly error: string | null;
	/** wrangler's output, kept when it failed. */
	readonly output: string | null;
	readonly started_at: string;
	readonly finished_at: string | null;
}

/** A run the deployer took, and a Worker it built, as `taken` answers them. */
export interface Taken {
	readonly run: number;
	readonly worker: string;
	/**
	 * A row of a run GitHub did not give, or of a Worker it did not download, for a reason of the
	 * moment: seen, and not taken.
	 */
	readonly transient?: boolean;
}

/** An app's release a run built, not yet written to the registry. */
export interface Unwritten {
	readonly repository: string;
	readonly run: number;
	readonly app: string;
	readonly commit: string | null;
}

const SCHEMA = `
CREATE TABLE IF NOT EXISTS deploys (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	action TEXT NOT NULL,
	worker TEXT NOT NULL,
	repository TEXT NOT NULL,
	run INTEGER,
	commit_sha TEXT,
	dry INTEGER NOT NULL,
	stage TEXT NOT NULL,
	failed_in TEXT,
	version TEXT,
	error TEXT,
	output TEXT,
	started_at TEXT NOT NULL,
	finished_at TEXT,
	transient INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS releases (
	repository TEXT NOT NULL,
	run INTEGER NOT NULL,
	app TEXT NOT NULL,
	commit_sha TEXT,
	written_at TEXT,
	outcome TEXT,
	PRIMARY KEY (repository, run, app)
);
`;

/**
 * How a run left unread for a reason of the moment was told before rows said so: what the deployer
 * logged of GitHub not reached, its rate limit spent, or a 5xx. Read once, by `transiently`.
 */
const TRANSIENT_BEFORE = [
	'% could not be reached for %',
	'%: the rate limit of % is spent%',
	'GitHub answered 5__ for %',
];

/**
 * A `deployer.db` from before rows said a run was unread for a reason of the moment gains the
 * column, each such row already there marked by what it says.
 */
function transiently(db: DatabaseSync): void {
	const columns = db.prepare(`SELECT name FROM pragma_table_info('deploys')`).all();
	if (columns.some((column) => column.name === 'transient')) return;
	db.exec('ALTER TABLE deploys ADD COLUMN transient INTEGER NOT NULL DEFAULT 0');
	const like = TRANSIENT_BEFORE.map(() => 'error LIKE ?').join(' OR ');
	db.prepare(`UPDATE deploys SET transient = 1 WHERE worker = '' AND (${like})`).run(
		...TRANSIENT_BEFORE,
	);
}

/** Still moving: a row in one of these when the deployer starts was left by one that stopped. */
const OPEN = ['downloading', 'admitting', 'uploading'];

interface Row {
	id: number;
	action: Action;
	worker: string;
	repository: string;
	run: number | null;
	commit_sha: string | null;
	dry: number;
	stage: Stage;
	failed_in: Stage | null;
	version: string | null;
	error: string | null;
	output: string | null;
	started_at: string;
	finished_at: string | null;
}

export class Store {
	private readonly db: DatabaseSync;
	private readonly now: () => Date;

	constructor(path: string, now: () => Date = () => new Date()) {
		this.now = now;
		this.db = new DatabaseSync(path);
		// The column first, so a table from before it is given it before anything reads it.
		const existed = this.db
			.prepare(`SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'deploys'`)
			.get();
		if (existed) transiently(this.db);
		this.db.exec(SCHEMA);
		// Nothing will finish a row a stopped deployer left open, so it is closed as failed.
		const open = OPEN.map(() => '?').join(', ');
		this.db
			.prepare(
				`UPDATE deploys SET failed_in = stage, stage = 'failed', finished_at = ?,
					error = 'the deployer stopped during it' WHERE stage IN (${open})`,
			)
			.run(this.stamp(), ...OPEN);
	}

	private stamp(): string {
		return this.now().toISOString();
	}

	/** A new deploy at `downloading`, and its id. */
	open(worker: string, repository: string, run: number, commit: string | null, dry: boolean) {
		const result = this.db
			.prepare(
				`INSERT INTO deploys (action, worker, repository, run, commit_sha, dry, stage, started_at)
					VALUES ('deploy', ?, ?, ?, ?, ?, 'downloading', ?)`,
			)
			.run(worker, repository, run, commit, dry ? 1 : 0, this.stamp());
		return Number(result.lastInsertRowid);
	}

	/**
	 * A run whose record or artifacts GitHub did not give, closed as failed where it was fetching:
	 * no Worker is known, so it names none and stores the empty name. `transient` where it was not
	 * given for a reason of the moment, so the run is not taken and catching up takes it again.
	 */
	unread(repository: string, run: number, dry: boolean, error: string, transient = false): number {
		const now = this.stamp();
		const result = this.db
			.prepare(
				`INSERT INTO deploys (action, worker, repository, run, dry, stage, failed_in, error,
					started_at, finished_at, transient)
					VALUES ('deploy', '', ?, ?, ?, 'failed', 'downloading', ?, ?, ?, ?)`,
			)
			.run(repository, run, dry ? 1 : 0, error, now, now, transient ? 1 : 0);
		return Number(result.lastInsertRowid);
	}

	/** A new rollback of `worker` to `version`, at `uploading` since there is nothing to fetch. */
	openRollback(worker: string, repository: string, version: string) {
		const result = this.db
			.prepare(
				`INSERT INTO deploys (action, worker, repository, dry, stage, version, started_at)
					VALUES ('rollback', ?, ?, 0, 'uploading', ?, ?)`,
			)
			.run(worker, repository, version, this.stamp());
		return Number(result.lastInsertRowid);
	}

	/**
	 * The newest run that deployed `worker` for real, or none. A run is taken only when it is newer,
	 * so a notice naming an old run can never roll a Worker back; a rollback leaves this unmoved.
	 */
	lastRun(worker: string): number | null {
		const row = this.db
			.prepare(
				`SELECT MAX(run) AS run FROM deploys
					WHERE worker = ? AND action = 'deploy' AND stage = 'deployed' AND dry = 0`,
			)
			.get(worker) as { run: number | null } | undefined;
		return row?.run ?? null;
	}

	/**
	 * Each run of `repository` it has a deploy or a release row of, with the Worker a deploy row
	 * names -- empty for a release, or a run GitHub never gave. A run with any row was taken,
	 * whatever came of it: catching up recovers notices, not deploys. A run GitHub did not give, or
	 * a Worker it did not download, for a reason of the moment is answered `transient`: seen, and not
	 * taken.
	 */
	taken(repository: string): Taken[] {
		const rows = this.db
			.prepare(
				`SELECT DISTINCT run, worker, transient FROM deploys
					WHERE repository = ? AND action = 'deploy' AND run IS NOT NULL
				UNION SELECT DISTINCT run, '', 0 FROM releases WHERE repository = ?`,
			)
			.all(repository, repository) as unknown as {
			run: number;
			worker: string;
			transient: number;
		}[];
		return rows.map(({ run, worker, transient }) =>
			transient ? { run, worker, transient: true } : { run, worker },
		);
	}

	/**
	 * Every app `run` built, each a release to write, but those `settled` names, each settled at once
	 * with why: an app rolled out by hand, `manual`. A run that built none keeps one row of no app,
	 * settled too, to say it was taken. Taken again, a run adds nothing.
	 */
	built(
		repository: string,
		run: number,
		commit: string | null,
		apps: readonly string[],
		settled: ReadonlyMap<string, string> = new Map(),
	): void {
		const insert = this.db.prepare(
			`INSERT OR IGNORE INTO releases (repository, run, app, commit_sha, written_at, outcome)
				VALUES (?, ?, ?, ?, ?, ?)`,
		);
		if (apps.length === 0) insert.run(repository, run, '', commit, this.stamp(), 'built nothing');
		for (const app of apps) {
			const outcome = settled.get(app);
			insert.run(repository, run, app, commit, outcome ? this.stamp() : null, outcome ?? null);
		}
	}

	/** The releases not yet written, the oldest run first. */
	unwritten(): Unwritten[] {
		return this.db
			.prepare(
				`SELECT repository, run, app, commit_sha AS "commit" FROM releases
					WHERE written_at IS NULL ORDER BY run, app`,
			)
			.all() as unknown as Unwritten[];
	}

	/** A release settled, with what came of it: written, or why it needed no write. */
	written(release: Unwritten, outcome: string): void {
		this.db
			.prepare(
				`UPDATE releases SET written_at = ?, outcome = ?
					WHERE repository = ? AND run = ? AND app = ?`,
			)
			.run(this.stamp(), outcome, release.repository, release.run, release.app);
	}

	advance(id: number, stage: 'admitting' | 'uploading'): void {
		this.db.prepare('UPDATE deploys SET stage = ? WHERE id = ?').run(stage, id);
	}

	deployed(id: number, version: string | null): void {
		this.db
			.prepare(
				`UPDATE deploys SET stage = 'deployed', version = COALESCE(?, version), finished_at = ?
					WHERE id = ?`,
			)
			.run(version, this.stamp(), id);
	}

	/** Close a row as left alone, with why: a Worker nobody owns, which is not a refusal. */
	skipped(id: number, reason: string): void {
		this.db
			.prepare(`UPDATE deploys SET stage = 'skipped', error = ?, finished_at = ? WHERE id = ?`)
			.run(reason, this.stamp(), id);
	}

	/**
	 * Close a row as failed where it was. `transient` where it failed for a reason of the moment --
	 * GitHub or its storage not answering for the artifact -- so catching up takes the Worker again.
	 */
	failed(id: number, error: string, output: string | null = null, transient = false): void {
		this.db
			.prepare(
				`UPDATE deploys SET failed_in = stage, stage = 'failed', error = ?, output = ?,
					finished_at = ?, transient = ? WHERE id = ?`,
			)
			.run(error, output, this.stamp(), transient ? 1 : 0, id);
	}

	/** Up to `limit` rows, the newest first, from before `before` when it is given. */
	list(limit = 50, before?: number): Deploy[] {
		const rows = this.db
			.prepare(`SELECT * FROM deploys WHERE (? IS NULL OR id < ?) ORDER BY id DESC LIMIT ?`)
			.all(before ?? null, before ?? null, limit) as unknown as Row[];
		return rows.map((row) => ({
			id: row.id,
			action: row.action,
			...(row.worker ? { worker: row.worker } : {}),
			repository: row.repository,
			run: row.run,
			commit: row.commit_sha,
			dry: row.dry === 1,
			stage: row.stage,
			failed_in: row.failed_in,
			version: row.version,
			error: row.error,
			output: row.output,
			started_at: row.started_at,
			finished_at: row.finished_at,
		}));
	}

	close(): void {
		this.db.close();
	}
}
