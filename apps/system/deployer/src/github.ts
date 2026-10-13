/**
 * What CI built, fetched: a run held to what a deploy requires, and its `worker-` artifacts
 * downloaded and held to the digest GitHub recorded. The notice that names a run is a hint; this is
 * where it becomes a decision. Ported from infra's libs/deploy/src/github.rs. See
 * spec/architecture/deployer.md, "One deployer, on one node".
 */
import { createHash } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import type { ReadableStream } from 'node:stream/web';
import { URLS } from '@monoflake/sdk';
import { tokenVariable } from './config.ts';

/** The workflow that builds what is deployed; a run of any other builds nothing to deploy. */
export const WORKFLOW = '.github/workflows/deploy.yml';
/** What may have started it. A pull request never does, and would not count if it did. */
const EVENTS = new Set(['push', 'schedule', 'workflow_dispatch']);
/** How an artifact carrying a Worker is named: `worker-console`. host takes `deploy-` alone. */
const PREFIX = 'worker-';
/** How an image for host is named: `deploy-<app>-<arch>`, one per architecture the run built. */
const IMAGE = /^deploy-(.+)-(?:arm64|amd64)$/;

/** A run, an artifact or a Worker the deployer will not deploy, and why. */
export class Refused extends Error {
	override name = 'Refused';
}

/**
 * What the deployer asked GitHub for and did not get, said as an operator acts on it: which run of
 * which repository, what was answered, and for a refused token, the variable it is read from.
 */
export class Unread extends Error {
	override name = 'Unread';
}

/**
 * What was not read for a reason of the moment rather than of the run: GitHub not reached, its rate
 * limit spent, or a 5xx. A run left unread so is not taken, and catching up takes it again.
 */
export class Transient extends Unread {
	override name = 'Transient';
}

/** GitHub's rate limit spent, and when it is given back, where GitHub said. */
export class RateLimited extends Transient {
	override name = 'RateLimited';
	readonly reset: Date | undefined;

	constructor(message: string, reset: Date | undefined) {
		super(message);
		this.reset = reset;
	}
}

/**
 * When a spent rate limit is given back: `x-ratelimit-reset`, in epoch seconds, or `retry-after`,
 * in seconds from `now`; undefined where the answer says neither, or is no rate limit at all.
 */
export function resetOf(answer: Response, now = new Date()): Date | undefined {
	const reset = Number(answer.headers.get('x-ratelimit-reset'));
	if (answer.headers.get('x-ratelimit-remaining') === '0' && reset > 0)
		return new Date(reset * 1000);
	const after = Number(answer.headers.get('retry-after'));
	return after > 0 ? new Date(now.getTime() + after * 1000) : undefined;
}

/** Whether `answer` says GitHub's rate limit is spent, the primary one or a secondary one. */
function limited(answer: Response): boolean {
	if (answer.status !== 403 && answer.status !== 429) return false;
	return (
		answer.status === 429 ||
		answer.headers.get('x-ratelimit-remaining') === '0' ||
		answer.headers.has('retry-after')
	);
}

/** The fields of GitHub's run record a deploy is decided on. */
export interface Run {
	readonly repository: { readonly full_name: string };
	readonly path: string;
	readonly head_branch: string | null;
	readonly event: string;
	readonly status: string;
	readonly conclusion: string | null;
	readonly head_sha?: string | null;
}

/** One Worker as a run left it. */
export interface Artifact {
	readonly app: string;
	readonly id: number;
	readonly digest: string;
}

/** Throws unless `record` is a finished, successful run of `repository`'s deploy.yml on `main`. */
export function check(run: number, record: Run, repository: string): void {
	const refuse = (why: string) => {
		throw new Refused(`run ${run} is not one to deploy: ${why}`);
	};
	if (record.repository.full_name !== repository) {
		refuse(`it belongs to ${record.repository.full_name}`);
	}
	if (record.path !== WORKFLOW) refuse(`it ran ${record.path}`);
	if (record.head_branch !== 'main') refuse(`it ran on ${record.head_branch}`);
	if (!EVENTS.has(record.event)) refuse(`a ${record.event} started it`);
	if (record.status !== 'completed' || record.conclusion !== 'success') {
		refuse(`it is ${record.status} with ${record.conclusion}`);
	}
}

/** A DNS label, as every app name is; see infra's spec/architecture/host.md. */
function isLabel(name: string): boolean {
	return name.length <= 63 && /^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/.test(name);
}

/**
 * The app a `worker-<app>` artifact name carries, when the name could be an app's. The name is
 * GitHub's answer, not this repository's, so it is held to the rule before it reaches a path.
 */
export function appOf(artifact: string): string | undefined {
	if (!artifact.startsWith(PREFIX)) return undefined;
	const app = artifact.slice(PREFIX.length);
	return isLabel(app) ? app : undefined;
}

/** The app a `deploy-<app>-<arch>` image artifact carries, held to the same rule. */
export function imageOf(artifact: string): string | undefined {
	const app = IMAGE.exec(artifact)?.[1];
	return app !== undefined && isLabel(app) ? app : undefined;
}

/** What a run built, as `artifacts` answers it. */
export interface Built {
	readonly commit: string | null;
	/** The Workers it built that can be deployed: neither expired nor missing a digest. */
	readonly artifacts: Artifact[];
	/**
	 * Every app it built an artifact of, a Worker or an image for host, each once and by name, so
	 * its release can be recorded -- RFC jln34.
	 */
	readonly built: string[];
	/** Of `built`, the apps it built an image of, for host; none where absent. */
	readonly images?: string[];
	/**
	 * The run's `declarations` artifact, each image's service.toml as `<app>.toml`, where it carries
	 * one: what says an app is rolled out by hand.
	 */
	readonly declarations?: Artifact;
}

/** The name of the artifact holding the declarations of every image a run built. */
export const DECLARATIONS = 'declarations';

type Fetch = (input: string, init?: RequestInit) => Promise<Response>;

/** An error and what it was caused by, as fetch throws `fetch failed` over the reason. */
function causeOf(error: unknown): string {
	if (!(error instanceof Error)) return String(error);
	return error.cause instanceof Error ? `${error.message}: ${error.cause.message}` : error.message;
}

/** The query that lists `workflow`'s successful runs on `main` created at `since` or later. */
export function recentPath(workflow: string, since: Date): string {
	const at = since.toISOString().replace(/\.\d{3}Z$/, 'Z');
	return `/actions/workflows/${workflow}/runs?branch=main&status=success&created=%3E%3D${at}&per_page=100`;
}

/** The ids of `runs` that are `repository`'s to deploy, the oldest first. */
export function deployable(runs: readonly (Run & { readonly id: number })[], repository: string) {
	const ids = runs.flatMap((record) => {
		try {
			check(record.id, record, repository);
			return [record.id];
		} catch {
			return [];
		}
	});
	return [...new Set(ids)].toSorted((a, b) => a - b);
}

interface Listed {
	readonly artifacts: readonly {
		readonly id: number;
		readonly name: string;
		readonly expired: boolean;
		readonly digest: string | null;
	}[];
}

export class GitHub {
	/** The token for a repository, picked by its owner; see config.ts, `tokenFor`. */
	private readonly tokenOf: (repository: string) => string | undefined;
	private readonly fetch: Fetch;

	constructor(
		tokenOf: (repository: string) => string | undefined,
		fetch: Fetch = globalThis.fetch,
	) {
		this.tokenOf = tokenOf;
		this.fetch = fetch;
	}

	/** A GET for `what`, with the token only when it is GitHub's own API being asked. */
	private async get(url: string, what: string, token?: string): Promise<Response> {
		const headers: Record<string, string> = {
			'user-agent': 'monoflake-deployer',
			accept: 'application/vnd.github+json',
			'x-github-api-version': '2022-11-28',
		};
		if (token) headers.authorization = `Bearer ${token}`;
		try {
			return await this.fetch(url, { headers, redirect: 'manual' });
		} catch (error) {
			const who = token ? 'GitHub' : 'storage';
			throw new Transient(`${who} could not be reached for ${what}: ${causeOf(error)}`);
		}
	}

	private token(repository: string): string {
		const token = this.tokenOf(repository);
		if (token) return token;
		const variable = tokenVariable(repository);
		const why = variable ? `${variable} is not set` : 'its owner names no variable';
		throw new Unread(`no GitHub token reads ${repository}: ${why}`);
	}

	/** Why `answer` is not `what` was asked for: the token named by its variable, never itself. */
	private unread(repository: string, what: string, answer: Response): Unread {
		const variable = tokenVariable(repository);
		const owner = repository.split('/')[0];
		const answered = `GitHub answered ${answer.status} for ${what}`;
		if (limited(answer)) {
			const reset = resetOf(answer);
			const until = reset ? ` until ${reset.toISOString()}` : '';
			return new RateLimited(`${answered}: the rate limit of ${variable} is spent${until}`, reset);
		}
		if (answer.status >= 500) return new Transient(answered);
		let why = '';
		if (answer.status === 401 || answer.status === 403) {
			why = `: ${variable}, the token for ${owner}, was refused`;
		} else if (answer.status === 404) {
			// GitHub answers 404, not 403, for a repository the token may not see.
			why = `: it is gone, or ${variable} may not read it`;
		}
		return new Unread(`${answered}${why}`);
	}

	private async json<T>(repository: string, path: string, what: string): Promise<T> {
		const url = `${URLS.external.github.api}/repos/${repository}${path}`;
		const answer = await this.get(url, what, this.token(repository));
		if (!answer.ok) throw this.unread(repository, what, answer);
		try {
			return (await answer.json()) as T;
		} catch (error) {
			throw new Unread(`GitHub's answer for ${what} could not be read: ${causeOf(error)}`);
		}
	}

	/**
	 * `repository`'s runs to deploy created within `within` milliseconds before `now`, the oldest
	 * first: one page, as host reads it, the last 100 being more than the gap between two passes.
	 */
	async recent(repository: string, within: number, now = new Date()): Promise<number[]> {
		const workflow = WORKFLOW.split('/').at(-1) ?? WORKFLOW;
		const path = recentPath(workflow, new Date(now.getTime() - within));
		const listed = await this.json<{ workflow_runs: (Run & { id: number })[] }>(
			repository,
			path,
			`the recent runs of ${repository}`,
		);
		return deployable(listed.workflow_runs, repository);
	}

	/** The `worker-` artifacts of `repository`'s `run`, once its record says it is one to deploy. */
	async artifacts(repository: string, run: number): Promise<Built> {
		const of = `run ${run} of ${repository}`;
		const record = await this.json<Run>(repository, `/actions/runs/${run}`, of);
		check(run, record, repository);
		const listed = await this.json<Listed>(
			repository,
			`/actions/runs/${run}/artifacts?per_page=100`,
			`the artifacts of ${of}`,
		);
		const artifacts = listed.artifacts.flatMap((artifact) => {
			const app = appOf(artifact.name);
			if (app === undefined || artifact.expired || !artifact.digest) return [];
			return [{ app, id: artifact.id, digest: artifact.digest }];
		});
		const named = listed.artifacts.map(({ name }) => appOf(name) ?? imageOf(name));
		const built = [...new Set(named.filter((app) => app !== undefined))].toSorted();
		const imaged = listed.artifacts.map(({ name }) => imageOf(name));
		const images = [...new Set(imaged.filter((app) => app !== undefined))].toSorted();
		const declared = listed.artifacts.find(
			(artifact) => artifact.name === DECLARATIONS && !artifact.expired && artifact.digest,
		);
		const declarations = declared && {
			app: DECLARATIONS,
			id: declared.id,
			digest: declared.digest as string,
		};
		return {
			commit: record.head_sha ?? null,
			artifacts,
			built,
			images,
			...(declarations ? { declarations } : {}),
		};
	}

	/**
	 * Download `artifact` of `run` to `zip`, held to its digest; the file is left either way. A
	 * Worker's is `worker-<app>`; the declarations are named as they are.
	 */
	async download(repository: string, run: number, artifact: Artifact, zip: string): Promise<void> {
		const url = `${URLS.external.github.api}/repos/${repository}/actions/artifacts/${artifact.id}/zip`;
		const name = artifact.app === DECLARATIONS ? DECLARATIONS : `worker-${artifact.app}`;
		const what = `artifact ${name} of run ${run} of ${repository}`;
		// GitHub answers with a redirect to storage, which is signed and must not be sent the token.
		const redirect = await this.get(url, what, this.token(repository));
		const location = redirect.headers.get('location');
		if (!location) throw this.unread(repository, what, redirect);
		const answer = await this.get(location, what);
		if (!answer.ok || !answer.body) {
			const Kind = answer.status >= 500 ? Transient : Unread;
			throw new Kind(`storage answered ${answer.status} for ${what}`);
		}
		const hash = createHash('sha256');
		const body = Readable.fromWeb(answer.body as ReadableStream<Uint8Array>);
		body.on('data', (chunk: Buffer) => hash.update(chunk));
		await pipeline(body, createWriteStream(zip));
		// GitHub's digest is the SHA-256 of the zip as downloaded, as infra's github.rs measured.
		const digest = `sha256:${hash.digest('hex')}`;
		if (digest.toLowerCase() !== artifact.digest.toLowerCase()) {
			throw new Refused(`artifact ${name} does not match its recorded digest`);
		}
	}
}
