import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { CatchUp, WITHIN_MS, missed, plan } from './catch-up.ts';
import { configOf } from './config.ts';
import { Deployer } from './deploy.ts';
import { PACKAGED, zipOf } from './fixtures.ts';
import { type Artifact, Unread } from './github.ts';
import { Store } from './store.ts';

const WEB = 'canmi21/web';
const ID = '0b7c2f1e-6c3a-4a7e-9b0d-1f2e3d4c5b6a';

/** The console's artifact as .mise/tasks/worker packages it. */
function consoleZip(): Buffer {
	const declaration = {
		name: 'console',
		main: 'bundle/_worker.js',
		no_bundle: true,
		compatibility_date: '2026-07-29',
	};
	const rest = PACKAGED.filter((item) => item.name !== 'wrangler.json');
	return zipOf([{ name: 'wrangler.json', data: JSON.stringify(declaration) }, ...rest]);
}

const CONSOLE: Artifact = { app: 'console', id: 1, digest: 'sha256:00' };

/**
 * A deployer and its catch-up over a GitHub listing `recent` runs, each run building what `built`
 * says, the console unless told; either throws where the option is an error.
 */
async function setup(
	options: {
		recent?: Record<string, number[] | Error>;
		built?: Record<number, Artifact[] | Error>;
	} = {},
) {
	const data = await mkdtemp(join(tmpdir(), 'deployer-'));
	const config = configOf({
		DEPLOYER_DATA: data,
		DEPLOY_SOURCES: 'monoflake/platform canmi21/web',
		WORKER_OWNERS: 'console=canmi21/web',
		WORKER_ZONES: 'canmi21/web=canmi.app',
		CLOUDFLARE_ACCOUNT_ID: 'account',
		CLOUDFLARE_WORKERS_TOKEN: 'token',
	});
	const store = new Store(join(data, 'deployer.db'));
	const asked: string[] = [];
	const github = {
		recent: async (repository: string, within: number) => {
			asked.push(`recent ${repository} ${within}`);
			const recent = options.recent?.[repository] ?? [];
			if (recent instanceof Error) throw recent;
			return recent;
		},
		artifacts: async (repository: string, run: number) => {
			asked.push(`artifacts ${repository} ${run}`);
			const built = options.built?.[run] ?? [CONSOLE];
			if (built instanceof Error) throw built;
			return { commit: 'abc', artifacts: built };
		},
		download: async (_repository: string, _run: number, _artifact: Artifact, zip: string) => {
			await writeFile(zip, consoleZip());
		},
	};
	/** While set, wrangler waits on it, so a run stays in flight. */
	const gate: { held?: Promise<void> } = {};
	const logs: string[] = [];
	const log = (line: string) => logs.push(line);
	const deployer = new Deployer({
		config,
		store,
		github,
		wrangler: async () => {
			await gate.held;
			return { code: 0, output: `Current Version ID: ${ID}` };
		},
		log,
	});
	const catchUp = new CatchUp({ sources: config.sources, store, github, deployer, log });
	/** A run of the console, deployed as a notice would have. */
	const took = async (run: number) => {
		deployer.notice({ run, repository: WEB });
		await deployer.idle();
	};
	/** Hold every wrangler from now until the returned function is called. */
	const hold = () => {
		let release: (() => void) | undefined;
		gate.held = new Promise((resolve) => {
			release = resolve;
		});
		return () => {
			gate.held = undefined;
			release?.();
		};
	};
	/** One pass, and every run it queued taken. */
	const pass = async () => {
		await catchUp.pass();
		await deployer.idle();
	};
	return { store, asked, logs, deployer, catchUp, took, hold, pass };
}

describe('missed', () => {
	it('is every recent run with no row, newer than the oldest held', () => {
		const taken = [5, 8].map((run) => ({ run, worker: 'console' }));
		expect(missed([3, 5, 6, 7, 8, 9], taken)).toEqual([6, 7, 9]);
	});

	it('is nothing while nothing of the repository is held', () => {
		expect(missed([3, 5, 6], [])).toEqual([]);
	});
});

describe('plan', () => {
	it('takes a Worker only from the newest run that built it, taken or missed', () => {
		const taken = [{ run: 8, worker: 'console' }];
		const planned = plan(taken, [
			[9, ['console', 'site']],
			[6, ['console', 'aka']],
			[7, ['site']],
		]);
		expect(planned.map(({ run, passedOver }) => [run, Object.fromEntries(passedOver)])).toEqual([
			[6, { console: 9 }],
			[7, { site: 9 }],
			[9, {}],
		]);
	});
});

describe('catching up', () => {
	it('takes a run whose notice never came, as a notice would, and says so', async () => {
		const { store, logs, took, pass } = await setup({ recent: { [WEB]: [5, 7] } });
		await took(5);
		await pass();
		expect(store.list().map((row) => [row.run, row.stage])).toEqual([
			[7, 'deployed'],
			[5, 'deployed'],
		]);
		expect(store.lastRun('console')).toBe(7);
		expect(logs).toContain(`deployer: run 7 of ${WEB} never reached the deployer; taking it now`);
	});

	it('leaves a run it took alone, asking GitHub nothing of it', async () => {
		const { store, asked, logs, took, pass } = await setup({ recent: { [WEB]: [5, 7] } });
		await took(5);
		await took(7);
		await pass();
		expect(store.list()).toHaveLength(2);
		expect(asked.filter((line) => line.startsWith('artifacts'))).toEqual([
			`artifacts ${WEB} 5`,
			`artifacts ${WEB} 7`,
		]);
		expect(logs.some((line) => line.includes('never reached'))).toBe(false);
	});

	it('deploys nothing twice, however often it passes', async () => {
		const { store, took, pass } = await setup({ recent: { [WEB]: [5, 7] } });
		await took(5);
		await pass();
		await pass();
		expect(store.list().map((row) => row.run)).toEqual([7, 5]);
	});

	it('leaves a run its notice is taking to the notice', async () => {
		const { store, asked, logs, deployer, catchUp, took, hold } = await setup({
			recent: { [WEB]: [5, 7] },
		});
		await took(5);
		const release = hold();
		deployer.notice({ run: 7, repository: WEB });
		await catchUp.pass();
		expect(deployer.inFlight(WEB, 7)).toBe(true);
		release();
		await deployer.idle();
		expect(deployer.inFlight(WEB, 7)).toBe(false);
		expect(asked.filter((line) => line === `artifacts ${WEB} 7`)).toHaveLength(1);
		expect(logs.some((line) => line.includes('never reached'))).toBe(false);
		expect(store.list().map((row) => [row.run, row.stage])).toEqual([
			[7, 'deployed'],
			[5, 'deployed'],
		]);
	});

	it('passes over an older missed build of a Worker a newer run built', async () => {
		const { store, logs, took, pass } = await setup({ recent: { [WEB]: [5, 6, 7] } });
		await took(5);
		await pass();
		expect(store.list().map((row) => [row.run, row.stage, row.error])).toEqual([
			[7, 'deployed', null],
			[6, 'skipped', 'run 7 built it again, and is the one deployed'],
			[5, 'deployed', null],
		]);
		expect(logs).toContain('deployer: console from run 6 is passed over for run 7');
	});

	it('asks once of a run that built no Worker, which leaves no row', async () => {
		const { asked, took, pass } = await setup({
			recent: { [WEB]: [5, 7] },
			built: { 7: [] },
		});
		await took(5);
		await pass();
		await pass();
		expect(asked.filter((line) => line === `artifacts ${WEB} 7`)).toHaveLength(1);
	});

	it('takes nothing of a repository it holds no run of', async () => {
		const { store, asked, pass } = await setup({ recent: { [WEB]: [5, 7] } });
		await pass();
		expect(store.list()).toEqual([]);
		expect(asked).toEqual([`recent monoflake/platform ${WITHIN_MS}`, `recent ${WEB} ${WITHIN_MS}`]);
	});

	it('goes on past a GitHub that does not answer, and takes the run next pass', async () => {
		const recent: Record<string, number[] | Error> = {
			'monoflake/platform': new Unread('GitHub answered 502 for the recent runs'),
			[WEB]: [5, 6, 7],
		};
		const built: Record<number, Artifact[] | Error> = {
			6: new Unread('GitHub answered 502 for run 6 of canmi21/web'),
		};
		const { store, logs, took, pass } = await setup({ recent, built });
		await took(5);
		await pass();
		expect(logs).toContain(
			'deployer: catching up on monoflake/platform: GitHub answered 502 for the recent runs',
		);
		expect(logs).toContain(
			`deployer: catching up on ${WEB} run 6: GitHub answered 502 for run 6 of canmi21/web`,
		);
		expect(store.list().map((row) => [row.run, row.stage])).toEqual([
			[7, 'deployed'],
			[5, 'deployed'],
		]);

		built[6] = [CONSOLE];
		await pass();
		expect(store.list()[0]).toMatchObject({ run: 6, stage: 'skipped' });
	});
});
