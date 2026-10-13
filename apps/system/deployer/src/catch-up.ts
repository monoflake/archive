/**
 * Runs a notice never brought: at start and every 15 minutes, the deployer lists each source's runs
 * to deploy from the last 7 days and takes the ones it has no record of, newer than the oldest it
 * holds, each Worker only from the newest run that built it. Ported from infra's
 * apps/deploy/host/src/rollout/catch_up.rs, so a lost notice changes nothing (RFC jln34). See
 * spec/architecture/deployer.md.
 */
import type { Deployer } from './deploy.ts';
import type { GitHub } from './github.ts';
import type { Store, Taken } from './store.ts';

/** How often the deployer looks for runs it missed, beyond once at start; host's period. */
export const EVERY_MS = 15 * 60 * 1000;

/** How far back: as long as CI keeps a run's artifacts. */
export const WITHIN_MS = 7 * 24 * 60 * 60 * 1000;

/** What to do with one missed run. */
export interface Planned {
	readonly run: number;
	/** Workers a newer run built too, each with that run: passed over, never deployed. */
	readonly passedOver: ReadonlyMap<string, number>;
}

/**
 * The runs among `recent` that the deployer missed: none it has a row of, and newer than the oldest
 * it holds. A deployer holding none of the repository's runs misses none, so one just set up does
 * not take the past week.
 */
export function missed(recent: readonly number[], taken: readonly Taken[]): number[] {
	const held = new Set(taken.map((each) => each.run));
	if (held.size === 0) return [];
	const oldest = Math.min(...held);
	return recent.filter((run) => run > oldest && !held.has(run)).toSorted((a, b) => a - b);
}

/**
 * What each missed run deploys, given the Workers each `built`, the oldest first: a Worker goes
 * from the newest run that built it, taken or missed, and from no other.
 */
export function plan(
	taken: readonly Taken[],
	built: readonly (readonly [number, readonly string[]])[],
): Planned[] {
	const newest = new Map<string, number>();
	const note = (worker: string, run: number) =>
		newest.set(worker, Math.max(newest.get(worker) ?? run, run));
	for (const { run, worker } of taken) if (worker) note(worker, run);
	for (const [run, workers] of built) for (const worker of workers) note(worker, run);
	return built
		.map(([run, workers]) => ({
			run,
			passedOver: new Map(
				workers.flatMap((worker) => {
					const newer = newest.get(worker) ?? run;
					return newer > run ? [[worker, newer] as const] : [];
				}),
			),
		}))
		.toSorted((a, b) => a.run - b.run);
}

export interface Parts {
	readonly sources: readonly string[];
	readonly store: Pick<Store, 'taken'>;
	readonly github: Pick<GitHub, 'recent' | 'artifacts'>;
	readonly deployer: Pick<Deployer, 'missed' | 'inFlight'>;
	readonly log?: (line: string) => void;
}

function messageOf(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

export class CatchUp {
	private readonly parts: Parts;
	/** The runs that built no Worker, which leave no row to say they were looked at. */
	private readonly empty = new Set<string>();

	constructor(parts: Parts) {
		this.parts = parts;
	}

	private log(line: string): void {
		(this.parts.log ?? console.log)(`deployer: ${line}`);
	}

	/**
	 * One look at every source, queueing each missed run as a notice would. A source or a run GitHub
	 * did not answer for is left to the next pass.
	 */
	async pass(): Promise<void> {
		const { sources, store, github, deployer } = this.parts;
		for (const repository of sources) {
			let recent: number[];
			try {
				// oxlint-disable-next-line no-await-in-loop -- one source at a time, as host does
				recent = await github.recent(repository, WITHIN_MS);
			} catch (error) {
				this.log(`catching up on ${repository}: ${messageOf(error)}`);
				continue;
			}
			const taken = store.taken(repository);
			const built: [number, string[]][] = [];
			for (const run of missed(recent, taken)) {
				const key = `${repository}#${run}`;
				if (this.empty.has(key) || deployer.inFlight(repository, run)) continue;
				let artifacts: { app: string }[];
				try {
					// oxlint-disable-next-line no-await-in-loop -- one run at a time, as host does
					({ artifacts } = await github.artifacts(repository, run));
				} catch (error) {
					this.log(`catching up on ${repository} run ${run}: ${messageOf(error)}`);
					continue;
				}
				if (artifacts.length === 0) this.empty.add(key);
				else built.push([run, artifacts.map((artifact) => artifact.app)]);
			}
			for (const { run, passedOver } of plan(taken, built)) {
				this.log(`run ${run} of ${repository} never reached the deployer; taking it now`);
				deployer.missed({ run, repository, passedOver });
			}
		}
	}
}

/** Run `pass` now and every `period` after the last one ended, one pass at a time. */
export function every(pass: () => Promise<void>, period = EVERY_MS): void {
	const loop = async () => {
		try {
			await pass();
		} catch (error) {
			console.log(`deployer: catching up: ${messageOf(error)}`);
		}
		setTimeout(loop, period);
	};
	void loop();
}
