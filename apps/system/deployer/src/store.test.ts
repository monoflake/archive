import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { describe, expect, it } from 'vitest';
import { Store } from './store.ts';

function path(): string {
	return join(mkdtempSync(join(tmpdir(), 'deployer-')), 'deployer.db');
}

const NOW = () => new Date('2026-10-06T12:00:00Z');

describe('the records', () => {
	it('move one row through its stages to deployed, with the version', () => {
		const store = new Store(path(), NOW);
		const id = store.open('console', 'canmi21/web', 7, 'abc', false);
		expect(store.list()[0]).toMatchObject({ id, stage: 'downloading', finished_at: null });
		store.advance(id, 'admitting');
		expect(store.list()[0]!.stage).toBe('admitting');
		store.advance(id, 'uploading');
		store.deployed(id, 'v-1');
		expect(store.list()).toEqual([
			{
				id,
				action: 'deploy',
				worker: 'console',
				repository: 'canmi21/web',
				run: 7,
				commit: 'abc',
				dry: false,
				stage: 'deployed',
				failed_in: null,
				version: 'v-1',
				error: null,
				output: null,
				started_at: '2026-10-06T12:00:00.000Z',
				finished_at: '2026-10-06T12:00:00.000Z',
			},
		]);
	});

	it('keep the stage a failed row failed in, its error and the output', () => {
		const store = new Store(path(), NOW);
		const id = store.open('console', 'canmi21/web', 7, null, true);
		store.advance(id, 'admitting');
		store.advance(id, 'uploading');
		store.failed(id, 'wrangler exited with 1', 'the output');
		expect(store.list()[0]).toMatchObject({
			dry: true,
			stage: 'failed',
			failed_in: 'uploading',
			error: 'wrangler exited with 1',
			output: 'the output',
		});
	});

	it('close a row a stopped deployer left open, as failed where it was', () => {
		const at = path();
		const first = new Store(at, NOW);
		const id = first.open('console', 'canmi21/web', 7, null, false);
		first.advance(id, 'admitting');
		const done = first.open('hook', 'monoflake/platform', 8, null, false);
		first.deployed(done, 'v');
		first.close();
		const again = new Store(at, NOW);
		const rows = again.list();
		expect(rows.find((row) => row.id === id)).toMatchObject({
			stage: 'failed',
			failed_in: 'admitting',
		});
		expect(rows.find((row) => row.id === done)!.stage).toBe('deployed');
	});

	it('know the newest run that deployed a Worker for real, never a dry one or a failure', () => {
		const store = new Store(path(), NOW);
		expect(store.lastRun('console')).toBeNull();
		store.deployed(store.open('console', 'canmi21/web', 7, null, false), 'v');
		store.deployed(store.open('console', 'canmi21/web', 12, null, true), null);
		store.failed(store.open('console', 'canmi21/web', 11, null, false), 'no');
		store.deployed(store.open('hook', 'monoflake/platform', 20, null, false), 'v');
		expect(store.lastRun('console')).toBe(7);
		store.deployed(store.openRollback('console', 'canmi21/web', 'v0'), null);
		expect(store.lastRun('console')).toBe(7);
		expect(store.list()[0]).toMatchObject({ action: 'rollback', run: null, version: 'v0' });
	});

	it('close a skipped row with its reason, as neither deployed nor failed', () => {
		const store = new Store(path(), NOW);
		store.skipped(store.open('hook', 'monoflake/platform', 7, null, false), 'nobody owns it');
		expect(store.list()[0]).toMatchObject({
			stage: 'skipped',
			failed_in: null,
			error: 'nobody owns it',
			finished_at: '2026-10-06T12:00:00.000Z',
		});
		expect(store.lastRun('hook')).toBeNull();
	});

	it('page the newest first', () => {
		const store = new Store(path(), NOW);
		const ids = [1, 2, 3].map((run) => store.open('hook', 'monoflake/platform', run, null, false));
		expect(store.list().map((row) => row.id)).toEqual(ids.toReversed());
		expect(store.list(1).map((row) => row.id)).toEqual([ids[2]]);
		expect(store.list(50, ids[2]).map((row) => row.id)).toEqual([ids[1], ids[0]]);
	});
});

describe('a run GitHub did not give for a reason of the moment', () => {
	it('is seen and not taken, where a run refused for good is taken', () => {
		const store = new Store(join(mkdtempSync(join(tmpdir(), 'deployer-')), 'deployer.db'));
		store.unread('monoflake/infra', 8, false, 'GitHub answered 403 for run 8: spent', true);
		store.unread('monoflake/infra', 9, false, 'GitHub answered 401 for run 9: refused');
		expect(store.taken('monoflake/infra')).toEqual([
			{ run: 8, worker: '', transient: true },
			{ run: 9, worker: '' },
		]);
	});

	it('is marked so in a deployer.db from before the column, by what its row says', () => {
		const file = path();
		const old = new DatabaseSync(file);
		old.exec(`CREATE TABLE deploys (
			id INTEGER PRIMARY KEY AUTOINCREMENT, action TEXT NOT NULL, worker TEXT NOT NULL,
			repository TEXT NOT NULL, run INTEGER, commit_sha TEXT, dry INTEGER NOT NULL,
			stage TEXT NOT NULL, failed_in TEXT, version TEXT, error TEXT, output TEXT,
			started_at TEXT NOT NULL, finished_at TEXT)`);
		const insert = old.prepare(
			`INSERT INTO deploys (action, worker, repository, run, dry, stage, error, started_at)
				VALUES ('deploy', ?, 'monoflake/infra', ?, 0, 'failed', ?, 'then')`,
		);
		const of = 'for run 1 of monoflake/infra';
		insert.run('', 1, `GitHub answered 403 ${of}: the rate limit of GITHUB_ACTIONS_TOKEN is spent`);
		insert.run('', 2, `GitHub could not be reached ${of}: fetch failed`);
		insert.run('', 3, `GitHub answered 502 ${of}`);
		insert.run(
			'',
			4,
			`GitHub answered 401 ${of}: GITHUB_ACTIONS_TOKEN, the token for x, was refused`,
		);
		insert.run('console', 5, `GitHub answered 502 for artifact worker-console`);
		old.close();
		const transient = new Store(file)
			.taken('monoflake/infra')
			.filter((each) => each.transient)
			.map((each) => each.run);
		expect(transient).toEqual([1, 2, 3]);
	});
});
