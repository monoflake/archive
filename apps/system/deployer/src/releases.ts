/**
 * Each app's release, written to the platform's registry: every app a run the deployer takes built
 * an artifact of, an image for host or a Worker, recorded as `{repository, run, commit}` on the
 * app's record, RFC jln34. A release only moves forward, the registry unreachable stops no deploy,
 * and a write not made is made on a later pass. See spec/architecture/deployer.md and
 * spec/architecture/registry.md.
 */
import { unwrap } from '@canmi/response';
import type { Store } from './store.ts';

/** One registry's time to answer before the next is asked, as host gives it. */
const PATIENCE_MS = 3000;

/** What the registry refuses of a release itself, never of the moment it was sent in. */
const FINAL = new Set(['invalid_body', 'forbidden_field', 'invalid_name']);

/** The code of a refusal outside the envelope, by its status: `http_405`. */
const UNENVELOPED = 'http_';

/** Who the registry records as having written a release. */
const BY = 'deployer';

/** What an app runs: the run of a repository that built it, and the commit it built. */
export interface Release {
	readonly repository: string;
	readonly run: number;
	readonly commit: string;
}

/** What came of one release. */
export type Outcome = 'written' | 'unchanged' | 'not newer' | 'no such app';

interface App {
	readonly revision: number;
	readonly spec: { readonly release?: Partial<Release> };
}

type Fetch = (input: string, init?: RequestInit) => Promise<Response>;

/** A registry that answered, refusing: an answer, never a reason to ask the next. */
class Refused extends Error {
	override name = 'Refused';
	readonly code: string;

	constructor(code: string, message: string) {
		super(message);
		this.code = code;
	}
}

/** Whether `release` should replace `recorded`: never by an older or the same run of one source. */
export function newer(release: Release, recorded: Partial<Release> | undefined): boolean {
	if (recorded?.repository !== release.repository || typeof recorded.run !== 'number') return true;
	return release.run > recorded.run;
}

/** `text` as JSON, or undefined where it is none, as an answer from no route of the registry's. */
function parsed(text: string): unknown {
	try {
		return JSON.parse(text);
	} catch {
		return undefined;
	}
}

/**
 * The registry's `/v1/apps`, asked of each core in turn, the one that answered last first: one
 * that cannot be reached, is too slow or fails on its own side passes the question on.
 */
export class Registry {
	private readonly cores: readonly string[];
	private readonly token: string | undefined;
	private readonly fetch: Fetch;
	private last: string | undefined;

	constructor(
		cores: readonly string[],
		token: string | undefined,
		fetch: Fetch = globalThis.fetch,
	) {
		this.cores = cores;
		this.token = token;
		this.fetch = fetch;
	}

	/** Whether it can write at all: a core to ask and a token to write with. */
	get configured(): boolean {
		return this.cores.length > 0 && this.token !== undefined;
	}

	private async ask<T>(path: string, init: RequestInit = {}): Promise<T> {
		const order = this.last
			? [this.last, ...this.cores.filter((at) => at !== this.last)]
			: this.cores;
		const headers = { authorization: `Bearer ${this.token}`, ...init.headers };
		let why = 'no registry is configured';
		for (const at of order) {
			let answer: Response;
			let text: string;
			try {
				// oxlint-disable-next-line no-await-in-loop -- each core only once the last failed
				answer = await this.fetch(`http://${at}${path}`, {
					...init,
					headers,
					signal: AbortSignal.timeout(PATIENCE_MS),
				});
				// oxlint-disable-next-line no-await-in-loop -- the same
				text = await answer.text();
			} catch (error) {
				why = `${at}: ${error instanceof Error ? error.message : String(error)}`;
				continue;
			}
			if (answer.status >= 500) {
				why = `${at}: it answered ${answer.status}`;
				continue;
			}
			this.last = at;
			const body = parsed(text);
			if (answer.ok) return unwrap<T>(body, `the registry at ${at}`);
			const { code, message = '' } = (body ?? {}) as { code?: unknown; message?: unknown };
			if (typeof code === 'string') {
				throw new Refused(code, `the registry at ${at} answered ${code}: ${String(message)}`);
			}
			// Not the envelope: a route the registry does not have, as a 405 says before PATCH is
			// deployed. Every core runs the same build, so it is the answer, not a core to pass over.
			const said = text.trim() ? `: ${text.trim().slice(0, 120)}` : ', with no body';
			throw new Refused(
				`${UNENVELOPED}${answer.status}`,
				`the registry at ${at} answered ${answer.status}${said}`,
			);
		}
		throw new Error(`no registry answered: ${why}`);
	}

	/**
	 * Record `release` as `app`'s, unless what it records is newer or the same, held to the revision
	 * read. A write another change got in before is read again and tried once more.
	 */
	async release(app: string, release: Release): Promise<Outcome> {
		const path = `/v1/apps/${encodeURIComponent(app)}`;
		for (let attempt = 0; ; attempt++) {
			let read: App;
			try {
				// oxlint-disable-next-line no-await-in-loop -- a retry reads again first
				read = await this.ask<App>(path);
			} catch (error) {
				if (error instanceof Refused && error.code === 'no_such_resource') return 'no such app';
				throw error;
			}
			if (!newer(release, read.spec.release)) return 'not newer';
			try {
				// oxlint-disable-next-line no-await-in-loop -- the same
				const wrote = await this.ask<{ revision?: number }>(path, {
					method: 'PATCH',
					headers: { 'content-type': 'application/json' },
					body: JSON.stringify({ by: BY, revision: read.revision, spec: { release } }),
				});
				return wrote.revision === undefined ? 'unchanged' : 'written';
			} catch (error) {
				const stale = error instanceof Refused && error.code === 'invalid_revision';
				if (!stale || attempt > 0) throw error;
			}
		}
	}
}

export interface Parts {
	readonly store: Pick<Store, 'unwritten' | 'written'>;
	readonly registry: Pick<Registry, 'configured' | 'release'>;
	readonly log?: (line: string) => void;
}

/**
 * The releases the store holds unwritten, written one at a time, oldest run first. One flush runs
 * at once; a flush asked for meanwhile runs once more after it, so nothing recorded waits a pass.
 */
export class Releases {
	private readonly parts: Parts;
	private flushing: Promise<void> | undefined;
	private again = false;

	constructor(parts: Parts) {
		this.parts = parts;
	}

	private log(line: string): void {
		(this.parts.log ?? console.log)(`deployer: ${line}`);
	}

	/** Write every release not yet written; never throws, and returns once it is done. */
	flush(): Promise<void> {
		if (this.flushing) {
			this.again = true;
			return this.flushing;
		}
		this.flushing = this.drain().finally(() => {
			this.flushing = undefined;
		});
		return this.flushing;
	}

	private async drain(): Promise<void> {
		do {
			this.again = false;
			try {
				// oxlint-disable-next-line no-await-in-loop -- once more when asked to meanwhile
				await this.writeAll();
			} catch (error) {
				this.log(`writing releases: ${error instanceof Error ? error.message : error}`);
			}
		} while (this.again);
	}

	private async writeAll(): Promise<void> {
		const { store, registry } = this.parts;
		if (!registry.configured) return;
		for (const unwritten of store.unwritten()) {
			const of = `${unwritten.app}'s release from run ${unwritten.run} of ${unwritten.repository}`;
			if (!unwritten.commit) {
				store.written(unwritten, 'no commit');
				this.log(`${of} names no commit, so it is not recorded`);
				continue;
			}
			const release = {
				repository: unwritten.repository,
				run: unwritten.run,
				commit: unwritten.commit,
			};
			let outcome: Outcome;
			try {
				// oxlint-disable-next-line no-await-in-loop -- one write at a time, oldest first
				outcome = await registry.release(unwritten.app, release);
			} catch (error) {
				// Left unwritten, for the next flush: a notice's or the next pass's. A registry that did
				// not answer, or lacks the route, answers the next the same, so the rest wait with it.
				const why = error instanceof Error ? error.message : String(error);
				// A body the registry refuses is refused again however often it is sent.
				if (error instanceof Refused && FINAL.has(error.code)) {
					store.written(unwritten, `refused: ${error.code}`);
					this.log(`${of} is not recorded: ${why}`);
					continue;
				}
				this.log(`${of} is not written yet: ${why}`);
				if (error instanceof Refused && !error.code.startsWith(UNENVELOPED)) continue;
				return;
			}
			store.written(unwritten, outcome);
			if (outcome === 'no such app')
				this.log(`${of} is not recorded: the registry has no ${unwritten.app}`);
			if (outcome === 'written') this.log(`${of} is recorded`);
		}
	}
}
