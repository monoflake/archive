import { mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { CatchUp, WITHIN_MS, missed, plan } from './catch-up.ts';
import { configOf } from './config.ts';
import { Deployer } from './deploy.ts';
import { PACKAGED, zipOf } from './fixtures.ts';
import { type Artifact, DECLARATIONS, RateLimited, Unread } from './github.ts';
import { type Registry, Releases } from './releases.ts';
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
const DECLARED: Artifact = { app: DECLARATIONS, id: 9, digest: 'sha256:00' };

/**
 * A deployer and its catch-up over a GitHub listing `recent` runs, each run building what `built`
 * says, the console unless told, and the `images` given, each declared as `declared` says, plainly
 * where it says nothing, and with no declarations at all where it says null; either of the first
 * two throws where the option is an error. Releases go to `registry`, where one is given.
 */
async function setup(
	options: {
		recent?: Record<string, number[] | Error>;
		built?: Record<number, Artifact[] | Error>;
		images?: Record<number, string[]>;
		declared?: Record<number, Record<string, string> | null>;
		registry?: Pick<Registry, 'configured' | 'release'>;
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
			const images = options.images?.[run] ?? [];
			const all = [...built.map((artifact) => artifact.app), ...images];
			const declarations = options.declared?.[run] === null ? {} : { declarations: DECLARED };
			return { commit: 'abc', artifacts: built, built: all, images, ...declarations };
		},
		download: async (_repository: string, run: number, artifact: Artifact, zip: string) => {
			if (artifact.app !== DECLARATIONS) return writeFile(zip, consoleZip());
			const images = options.images?.[run] ?? [];
			const declared = options.declared?.[run] ?? {};
			const entries = images.map((app) => ({
				name: `${app}.toml`,
				data: declared[app] ?? `name = "${app}"\nplacements = ["tyo"]\n`,
			}));
			return writeFile(zip, zipOf(entries));
		},
	};
	/** While set, wrangler waits on it, so a run stays in flight. */
	const gate: { held?: Promise<void> } = {};
	const logs: string[] = [];
	const log = (line: string) => logs.push(line);
	const releases = options.registry && new Releases({ store, registry: options.registry, log });
	const deployer = new Deployer({
		config,
		store,
		github,
		...(releases ? { releases } : {}),
		wrangler: async () => {
			await gate.held;
			return { code: 0, output: `Current Version ID: ${ID}` };
		},
		log,
	});
	const catchUp = new CatchUp({
		sources: config.sources,
		store,
		github,
		deployer,
		log,
		...(releases ? { releases } : {}),
	});
	/** A run of the console, deployed as a notice would have. */
	const took = async (run: number) => {
		deployer.notice({ run, repository: WEB });
		await deployer.idle();
		await releases?.flush();
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
	/** One pass, and every run it queued taken, its releases flushed. */
	const pass = async () => {
		await catchUp.pass();
		await deployer.idle();
		await releases?.flush();
	};
	return { store, asked, logs, deployer, catchUp, took, hold, pass };
}

describe('missed', () => {
	it('is every recent run with no row, newer than the oldest held', () => {
		const taken = [5, 8].map((run) => ({ run, worker: 'console' }));
		expect(missed([3, 5, 6, 7, 8, 9], taken)).toEqual([6, 7, 9]);
	});

	it('is a run GitHub did not give for a reason of the moment, the only one held among them', () => {
		const unread = [{ run: 8, worker: '', transient: true }];
		expect(missed([3, 8, 9], unread)).toEqual([8, 9]);
		expect(missed([8], [...unread, { run: 8, worker: 'console' }])).toEqual([]);
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

/** A registry that records each release it is asked to write, and fails while `down` is set. */
function registry() {
	const written: [string, number][] = [];
	const state = { down: false };
	const fake: Pick<Registry, 'configured' | 'release'> = {
		configured: true,
		release: async (app, release) => {
			if (state.down) throw new Error('no registry answered: 100.64.0.1:12013: fetch failed');
			written.push([app, release.run]);
			return 'written';
		},
	};
	return { fake, written, state };
}

describe('releases', () => {
	it('records every app a run built, image or Worker, once its run is taken', async () => {
		const { fake, written } = registry();
		const { store, took } = await setup({ registry: fake, images: { 5: ['geo', 'relay'] } });
		await took(5);
		expect(written).toEqual([
			['console', 5],
			['geo', 5],
			['relay', 5],
		]);
		expect(store.unwritten()).toEqual([]);
	});

	it('deploys while the registry is down, and writes the release on the next pass', async () => {
		const { fake, written, state } = registry();
		const { store, logs, took, pass } = await setup({
			registry: fake,
			recent: { [WEB]: [5] },
			images: { 5: ['geo'] },
		});
		state.down = true;
		await took(5);
		expect(store.list()).toMatchObject([{ run: 5, worker: 'console', stage: 'deployed' }]);
		expect(written).toEqual([]);
		expect(logs).toContain(
			`deployer: console's release from run 5 of ${WEB} is not written yet: no registry answered: 100.64.0.1:12013: fetch failed`,
		);
		// The registry not answering, the rest wait rather than each asking it again.
		expect(logs.some((line) => line.includes("geo's release"))).toBe(false);
		expect(store.unwritten().map((release) => release.app)).toEqual(['console', 'geo']);

		state.down = false;
		await pass();
		expect(written).toEqual([
			['console', 5],
			['geo', 5],
		]);
		expect(store.unwritten()).toEqual([]);
	});

	it('takes a run that built images alone, for its releases, and holds it taken', async () => {
		const { fake, written } = registry();
		const { asked, took, pass } = await setup({
			registry: fake,
			recent: { [WEB]: [5, 7] },
			built: { 7: [] },
			images: { 7: ['geo'] },
		});
		await took(5);
		await pass();
		await pass();
		expect(written).toEqual([
			['console', 5],
			['geo', 7],
		]);
		expect(asked.filter((line) => line === `artifacts ${WEB} 7`)).toHaveLength(2);
	});

	it('writes no release for an app rolled out by hand, and the rest of its run as ever', async () => {
		const { fake, written } = registry();
		const { store, took } = await setup({
			registry: fake,
			images: { 5: ['database', 'geo'] },
			declared: { 5: { database: 'name = "database"\nrollout = "manual"\n' } },
		});
		await took(5);
		expect(written).toEqual([
			['console', 5],
			['geo', 5],
		]);
		expect(store.unwritten()).toEqual([]);
	});

	it("moves no image's release where the run's declarations cannot be read", async () => {
		const { fake, written } = registry();
		const { store, logs, took } = await setup({
			registry: fake,
			images: { 5: ['geo'] },
			declared: { 5: null },
		});
		await took(5);
		expect(store.list()).toMatchObject([{ run: 5, worker: 'console', stage: 'deployed' }]);
		expect(written).toEqual([['console', 5]]);
		expect(logs).toContain(
			"deployer: run 5: its images' releases are not recorded: it carries no declarations",
		);
	});
});

describe('a run GitHub did not give', () => {
	const SPENT = 'GitHub answered 403 for run 7 of canmi21/web: the rate limit of X is spent';

	it('is taken again on the next pass when its rate limit was spent', async () => {
		const built: Record<number, Artifact[] | Error> = {
			7: new RateLimited(SPENT, new Date('2026-10-13T06:00:35Z')),
		};
		const { store, logs, took, pass } = await setup({ recent: { [WEB]: [5, 7] }, built });
		await took(5);
		await took(7);
		expect(store.list()[0]).toMatchObject({ run: 7, stage: 'failed', error: SPENT });

		built[7] = [CONSOLE];
		await pass();
		expect(logs).toContain(`deployer: run 7 of ${WEB} was not read from GitHub; taking it now`);
		expect(store.list()[0]).toMatchObject({ run: 7, worker: 'console', stage: 'deployed' });
		await pass();
		expect(store.list().filter((row) => row.run === 7)).toHaveLength(2);
	});

	it('stays taken when GitHub refused it for good', async () => {
		const refused = new Unread('GitHub answered 401 for run 7 of canmi21/web: X was refused');
		const built: Record<number, Artifact[] | Error> = { 7: refused };
		const { store, asked, took, pass } = await setup({ recent: { [WEB]: [5, 7] }, built });
		await took(5);
		await took(7);
		built[7] = [CONSOLE];
		await pass();
		expect(asked.filter((line) => line === `artifacts ${WEB} 7`)).toHaveLength(1);
		expect(store.list()[0]).toMatchObject({ run: 7, stage: 'failed' });
	});

	it('ends a pass at the first spent rate limit, queueing nothing after it', async () => {
		const spent = new RateLimited(SPENT, undefined);
		const { store, asked, logs, took, pass } = await setup({
			recent: { [WEB]: [5, 6, 7, 8] },
			built: { 7: spent },
		});
		await took(5);
		await pass();
		expect(asked.filter((line) => line.startsWith('artifacts'))).toEqual([
			`artifacts ${WEB} 5`,
			`artifacts ${WEB} 6`,
			`artifacts ${WEB} 7`,
		]);
		expect(store.list().map((row) => row.run)).toEqual([5]);
		expect(logs).toContain(`deployer: catching up stops until the next pass: ${SPENT}`);
	});

	it('asks no other source once the list of one is refused for its rate limit', async () => {
		const { asked, logs, pass, took } = await setup({
			recent: { 'monoflake/platform': new RateLimited(SPENT, undefined), [WEB]: [5, 7] },
		});
		await took(5);
		await pass();
		expect(asked.filter((line) => line.startsWith('recent'))).toEqual([
			`recent monoflake/platform ${WITHIN_MS}`,
		]);
		expect(logs.some((line) => line.includes('stops until the next pass'))).toBe(true);
	});
});
