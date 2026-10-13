/**
 * A run's artifacts read from the S3-compatible bucket CI uploads them to, the run itself still
 * checked with GitHub -- RFC q28g5. Only the organization's repositories upload there, so a run the
 * bucket holds nothing of is read from GitHub, as before. Each is
 * `<owner>/<repo>/<run>/<artifact>.zip`, zipped as GitHub zipped it, with `<artifact>.json` beside
 * it holding its `sha256` and `size`; one without it, or not matching it, is not used, and one
 * deleted is passed over as an expired one is. Reached by the S3 API alone, signed with AWS's
 * Signature Version 4. See spec/architecture/deployer.md.
 */
import { createHash, createHmac } from 'node:crypto';
import type { BucketAccess } from './config.ts';
import {
	type Artifact,
	type Built,
	DECLARATIONS,
	type GitHub,
	Refused,
	Transient,
	Unread,
	appOf,
	causeOf,
	imageOf,
	nameOf,
	save,
} from './github.ts';

type Fetch = (input: string, init?: RequestInit) => Promise<Response>;

/** The SHA-256 of nothing, the payload of every request it makes. */
const EMPTY = createHash('sha256').update('').digest('hex');

/** Pages of a listing read before it stops; a run's objects fit in one. */
const PAGES = 10;

/** `text` percent-encoded as SigV4 requires: RFC 3986's unreserved characters alone kept. */
function encode(text: string): string {
	return encodeURIComponent(text).replace(
		/[!'()*]/g,
		(char) => `%${char.charCodeAt(0).toString(16).toUpperCase()}`,
	);
}

function hmac(key: string | Buffer, text: string): Buffer {
	return createHmac('sha256', key).update(text).digest();
}

/** A signed GET of `key` in the bucket, with `query`, as at `now`. */
export function signed(
	access: BucketAccess,
	key: string,
	query: Readonly<Record<string, string>>,
	now: Date,
): { url: string; headers: Record<string, string> } {
	const endpoint = new URL(access.endpoint);
	const base = endpoint.pathname.replace(/\/+$/, '');
	const host = access.virtualHost ? `${access.bucket}.${endpoint.host}` : endpoint.host;
	const segments = access.virtualHost ? [key] : key ? [access.bucket, key] : [access.bucket];
	const path = `${base}/${segments.join('/').split('/').map(encode).join('/')}`;
	const search = Object.entries(query)
		.map(([name, value]) => `${encode(name)}=${encode(value)}`)
		.toSorted()
		.join('&');
	const date = now
		.toISOString()
		.replace(/[-:]/g, '')
		.replace(/\.\d{3}/, '');
	const day = date.slice(0, 8);
	const scope = `${day}/${access.region}/s3/aws4_request`;
	const names = 'host;x-amz-content-sha256;x-amz-date';
	const request = [
		'GET',
		path,
		search,
		`host:${host}`,
		`x-amz-content-sha256:${EMPTY}`,
		`x-amz-date:${date}`,
		'',
		names,
		EMPTY,
	].join('\n');
	const hashed = createHash('sha256').update(request).digest('hex');
	const toSign = ['AWS4-HMAC-SHA256', date, scope, hashed].join('\n');
	const signing = ['s3', 'aws4_request'].reduce(
		(at, part) => hmac(at, part),
		hmac(hmac(`AWS4${access.secret}`, day), access.region),
	);
	const signature = createHmac('sha256', signing).update(toSign).digest('hex');
	return {
		url: `${endpoint.protocol}//${host}${path}${search ? `?${search}` : ''}`,
		headers: {
			'x-amz-content-sha256': EMPTY,
			'x-amz-date': date,
			authorization: `AWS4-HMAC-SHA256 Credential=${access.keyId}/${scope}, SignedHeaders=${names}, Signature=${signature}`,
		},
	};
}

const ENTITIES: Record<string, string> = {
	amp: '&',
	lt: '<',
	gt: '>',
	quot: '"',
	apos: "'",
};

function unescaped(xml: string): string {
	return xml.replace(/&(\w+);/g, (whole, name: string) => ENTITIES[name] ?? whole);
}

/** One page of a ListObjectsV2 answer: each key with its size, and the token for the next. */
export function pageOf(xml: string): { objects: Map<string, number>; next: string | undefined } {
	const objects = new Map<string, number>();
	for (const [, contents = ''] of xml.matchAll(/<Contents>([\s\S]*?)<\/Contents>/g)) {
		const key = /<Key>([\s\S]*?)<\/Key>/.exec(contents)?.[1];
		const size = Number(/<Size>(\d+)<\/Size>/.exec(contents)?.[1]);
		if (key !== undefined && Number.isSafeInteger(size)) objects.set(unescaped(key), size);
	}
	const truncated = /<IsTruncated>true<\/IsTruncated>/.test(xml);
	const next = /<NextContinuationToken>([\s\S]*?)<\/NextContinuationToken>/.exec(xml)?.[1];
	return { objects, next: truncated && next ? unescaped(next) : undefined };
}

/** What `<artifact>.json` says of its zip, where it says it plainly; undefined otherwise. */
export function recordOf(json: unknown): { digest: string; size: number } | undefined {
	if (typeof json !== 'object' || json === null) return undefined;
	const { sha256, size } = json as Record<string, unknown>;
	if (typeof sha256 !== 'string' || !/^[0-9a-f]{64}$/i.test(sha256)) return undefined;
	if (typeof size !== 'number' || !Number.isSafeInteger(size) || size < 0) return undefined;
	return { digest: `sha256:${sha256.toLowerCase()}`, size };
}

export class Bucket {
	private readonly access: BucketAccess;
	private readonly github: Pick<GitHub, 'run' | 'recent' | 'artifacts' | 'download'>;
	private readonly fetch: Fetch;
	private readonly now: () => Date;

	constructor(
		access: BucketAccess,
		github: Pick<GitHub, 'run' | 'recent' | 'artifacts' | 'download'>,
		fetch: Fetch = globalThis.fetch,
		now: () => Date = () => new Date(),
	) {
		this.access = access;
		this.github = github;
		this.fetch = fetch;
		this.now = now;
	}

	/** GitHub's, as ever: which runs succeeded is GitHub's to say. */
	recent(repository: string, within: number): Promise<number[]> {
		return this.github.recent(repository, within);
	}

	private async get(key: string, what: string, query: Record<string, string> = {}) {
		const { url, headers } = signed(this.access, key, query, this.now());
		let answer: Response;
		try {
			answer = await this.fetch(url, { headers });
		} catch (error) {
			throw new Transient(`the bucket could not be reached for ${what}: ${causeOf(error)}`);
		}
		return answer;
	}

	/** Why `answer` is not `what` was asked for, with the code S3 answered where it gave one. */
	private async unread(what: string, answer: Response): Promise<Unread> {
		const body = await answer.text().catch(() => '');
		const code = /<Code>([^<]*)<\/Code>/.exec(body)?.[1];
		const answered = `the bucket answered ${answer.status} for ${what}${code ? `: ${code}` : ''}`;
		if (answer.status >= 500 || answer.status === 429) return new Transient(answered);
		if (answer.status === 401 || answer.status === 403) {
			return new Unread(`${answered}; ARTIFACTS_READ_KEY_ID may not read it`);
		}
		return new Unread(answered);
	}

	/** Every object under `prefix`, with its size. */
	private async list(prefix: string, what: string): Promise<Map<string, number>> {
		const objects = new Map<string, number>();
		let next: string | undefined;
		for (let page = 0; page < PAGES; page += 1) {
			const query: Record<string, string> = { 'list-type': '2', prefix };
			if (next) query['continuation-token'] = next;
			// oxlint-disable-next-line no-await-in-loop -- each page names the next
			const answer = await this.get('', what, query);
			// oxlint-disable-next-line no-await-in-loop -- as above
			if (!answer.ok) throw await this.unread(what, answer);
			// oxlint-disable-next-line no-await-in-loop -- as above
			const listed = pageOf(await answer.text());
			for (const [key, size] of listed.objects) objects.set(key, size);
			next = listed.next;
			if (!next) break;
		}
		return objects;
	}

	/** What `<name>.json` says of `<name>.zip`, listed at `size`; undefined where unusable. */
	private async record(prefix: string, name: string, size: number, of: string) {
		const what = `${name}.json of ${of}`;
		const answer = await this.get(`${prefix}${name}.json`, what);
		if (answer.status === 404) return undefined;
		if (!answer.ok) throw await this.unread(what, answer);
		const record = recordOf(await answer.json().catch(() => undefined));
		return record?.size === size ? record : undefined;
	}

	/**
	 * The Workers `repository`'s `run` left in the bucket, once GitHub says the run is one to deploy,
	 * or GitHub's own where the bucket holds nothing of the run. An artifact counts only with its
	 * `.json` beside it, and a Worker is deployable only where that names its digest and the size it
	 * was listed at.
	 */
	async artifacts(repository: string, run: number): Promise<Built> {
		const of = `run ${run} of ${repository}`;
		const prefix = `${repository}/${run}/`;
		// Listed before the run is checked, so a run the bucket does not hold asks GitHub once.
		const objects = await this.list(prefix, `the artifacts of ${of}`);
		if (objects.size === 0) return this.github.artifacts(repository, run);
		const record = await this.github.run(repository, run);
		const zips = new Map<string, number>();
		for (const [key, size] of objects) {
			const name = key.slice(prefix.length).replace(/\.zip$/, '');
			if (key.endsWith('.zip') && !name.includes('/') && objects.has(`${prefix}${name}.json`)) {
				zips.set(name, size);
			}
		}
		const read = async (app: string, name: string): Promise<Artifact[]> => {
			const size = zips.get(name);
			const recorded = size === undefined ? undefined : await this.record(prefix, name, size, of);
			return recorded ? [{ app, ...recorded }] : [];
		};
		const names = [...zips.keys()];
		const workers = names.flatMap((name) => {
			const app = appOf(name);
			return app === undefined ? [] : [[app, name] as const];
		});
		const artifacts: Artifact[] = [];
		for (const [app, name] of workers) {
			// oxlint-disable-next-line no-await-in-loop -- one request at a time, as GitHub is asked
			artifacts.push(...(await read(app, name)));
		}
		const [declarations] = await read(DECLARATIONS, DECLARATIONS);
		const named = names.map((name) => appOf(name) ?? imageOf(name));
		const built = [...new Set(named.filter((app) => app !== undefined))].toSorted();
		const imaged = names.map((name) => imageOf(name));
		const images = [...new Set(imaged.filter((app) => app !== undefined))].toSorted();
		return {
			commit: record.head_sha ?? null,
			artifacts: artifacts.toSorted((a, b) => a.app.localeCompare(b.app)),
			built,
			images,
			...(declarations ? { declarations } : {}),
		};
	}

	/** Download `artifact` of `run` to `zip`, held to its digest and size; the file is left. */
	async download(repository: string, run: number, artifact: Artifact, zip: string): Promise<void> {
		// GitHub's id marks an artifact of a run the bucket held nothing of.
		if (artifact.id !== undefined) return this.github.download(repository, run, artifact, zip);
		const name = nameOf(artifact);
		const what = `artifact ${name} of run ${run} of ${repository}`;
		const answer = await this.get(`${repository}/${run}/${name}.zip`, what);
		if (answer.status === 404) throw new Unread(`${what} is no longer kept in the bucket`);
		if (!answer.ok || !answer.body) throw await this.unread(what, answer);
		const { digest, size } = await save(answer.body, zip);
		if (digest !== artifact.digest.toLowerCase()) {
			throw new Refused(`artifact ${name} does not match its recorded digest`);
		}
		if (artifact.size !== undefined && size !== artifact.size) {
			throw new Refused(`artifact ${name} is not the size recorded`);
		}
	}
}
