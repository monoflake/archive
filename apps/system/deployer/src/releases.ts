/**
 * Each app's release, written to the platform's control: every app a run the deployer takes built
 * an artifact of, an image for host or a Worker, recorded as `{repository, run, commit}` on the
 * app's record, RFC jln34. A release only moves forward, control unreachable stops no deploy, and a
 * write not made is made on a later pass. See spec/architecture/deployer.md and
 * spec/architecture/control.md, "It takes writes as the old registry did".
 */
import { unwrap } from '@canmi/response';
import type { Store } from './store.ts';

/** Control's time to answer, over HTTPS to another account's Worker. */
const PATIENCE_MS = 10_000;

/** What control refuses of a release itself, never of the moment it was sent in. */
const FINAL = new Set(['invalid_body', 'forbidden_field', 'invalid_name']);

/** Who control records as having written a release. */
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

/** Control answered, refusing in its envelope: an answer about this release, not about control. */
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

/** `text` as JSON, or undefined where it is none, as an answer from in front of control. */
function parsed(text: string): unknown {
	try {
		return JSON.parse(text);
	} catch {
		return undefined;
	}
}

/** Control's `/v1/registry/apps`, at `CONTROL`, written with `CONTROL_WRITE_TOKEN`. */
export class Control {
	private readonly base: string | undefined;
	private readonly token: string | undefined;
	private readonly fetch: Fetch;

	constructor(
		base: string | undefined,
		token: string | undefined,
		fetch: Fetch = globalThis.fetch,
	) {
		this.base = base?.replace(/\/+$/, '');
		this.token = token;
		this.fetch = fetch;
	}

	/** Whether it can write at all: an address to ask and a token to write with. */
	get configured(): boolean {
		return Boolean(this.base) && this.token !== undefined;
	}

	private async ask<T>(path: string, init: RequestInit = {}): Promise<T> {
		const headers = { authorization: `Bearer ${this.token}`, ...init.headers };
		let answer: Response;
		let text: string;
		try {
			answer = await this.fetch(`${this.base}${path}`, {
				...init,
				headers,
				signal: AbortSignal.timeout(PATIENCE_MS),
			});
			text = await answer.text();
		} catch (error) {
			const why = error instanceof Error ? error.message : String(error);
			throw new Error(`control did not answer: ${why}`, { cause: error });
		}
		const body = parsed(text);
		if (answer.ok) return unwrap<T>(body, 'control');
		const { code, message = '' } = (body ?? {}) as { code?: unknown; message?: unknown };
		// Its own side failing, or an answer from in front of it, is about control, so the rest wait.
		if (answer.status >= 500 || typeof code !== 'string') {
			const said = typeof code === 'string' ? ` ${code}` : '';
			throw new Error(`control answered ${answer.status}${said}`);
		}
		throw new Refused(code, `control answered ${code}: ${String(message)}`);
	}

	/**
	 * Record `release` as `app`'s, unless what it records is newer or the same, held to the revision
	 * read. A write another change got in before is read again and tried once more.
	 */
	async release(app: string, release: Release): Promise<Outcome> {
		const path = `/v1/registry/apps/${encodeURIComponent(app)}`;
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
	readonly control: Pick<Control, 'configured' | 'release'>;
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
		const { store, control } = this.parts;
		if (!control.configured) return;
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
				outcome = await control.release(unwritten.app, release);
			} catch (error) {
				// Left unwritten, for the next flush: a notice's or the next pass's. Control not
				// answering answers the next the same, so the rest wait with it.
				const why = error instanceof Error ? error.message : String(error);
				// A body control refuses is refused again however often it is sent.
				if (error instanceof Refused && FINAL.has(error.code)) {
					store.written(unwritten, `refused: ${error.code}`);
					this.log(`${of} is not recorded: ${why}`);
					continue;
				}
				this.log(`${of} is not written yet: ${why}`);
				if (error instanceof Refused) continue;
				return;
			}
			store.written(unwritten, outcome);
			if (outcome === 'no such app')
				this.log(`${of} is not recorded: control has no ${unwritten.app}`);
			if (outcome === 'written') this.log(`${of} is recorded`);
		}
	}
}
