import { createHash } from 'node:crypto';
import { mkdtemp, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { Bucket, pageOf, recordOf, signed } from './bucket.ts';
import { CatchUp } from './catch-up.ts';
import { type BucketAccess, bucketMissing, configOf } from './config.ts';
import { Deployer } from './deploy.ts';
import { PACKAGED, zipOf } from './fixtures.ts';
import { type Artifact, DECLARATIONS, Refused, Transient, Unread } from './github.ts';
import { Store } from './store.ts';

const ACCESS: BucketAccess = {
	endpoint: 'http://localhost:9000',
	region: 'us-east-1',
	bucket: 'artifacts',
	keyId: 'reader',
	secret: 'secret',
	virtualHost: false,
};

const PLATFORM = 'monoflake/platform';
/** The console as GitHub keeps it, for a repository that does not upload to the bucket. */
const GITHUB: Artifact = { app: 'console', id: 1, digest: 'sha256:00' };
const RECORD = { repository: { full_name: PLATFORM }, head_sha: 'abc' };

function sha256(data: string): string {
	return createHash('sha256').update(data).digest('hex');
}

/** `<name>.zip` holding `data`, with the `.json` CI writes beside it, under run 7 of web. */
function uploaded(name: string, data: string, json: object = {}): [string, string][] {
	const record = { sha256: sha256(data), size: Buffer.byteLength(data), ...json };
	return [
		[`${PLATFORM}/7/${name}.zip`, data],
		[`${PLATFORM}/7/${name}.json`, JSON.stringify(record)],
	];
}

/**
 * A bucket served path-style from `objects`, each request asked recorded; `answer` stands in for
 * any request it answers, or throws as fetch does.
 */
function bucketOf(
	objects: Map<string, string>,
	answer: (key: string) => Response | undefined = () => undefined,
	checked: (run: number) => void = () => {},
) {
	const asked: string[] = [];
	const fetch = async (input: string) => {
		const url = new URL(input);
		const key = decodeURIComponent(url.pathname.replace(/^\/artifacts\/?/, ''));
		asked.push(url.search ? `list ${url.searchParams.get('prefix')}` : key);
		const instead = answer(key);
		if (instead) return instead;
		if (url.searchParams.get('list-type') === '2') {
			const prefix = url.searchParams.get('prefix') ?? '';
			const contents = [...objects]
				.filter(([name]) => name.startsWith(prefix))
				.map(([name, data]) => {
					const size = Buffer.byteLength(data);
					return `<Contents><Key>${name}</Key><Size>${size}</Size></Contents>`;
				});
			return new Response(`<ListBucketResult>${contents.join('')}</ListBucketResult>`);
		}
		const data = objects.get(key);
		if (data === undefined)
			return new Response('<Error><Code>NoSuchKey</Code></Error>', { status: 404 });
		return new Response(data);
	};
	const github = {
		run: async (repository: string, run: number) => {
			checked(run);
			return { ...RECORD, repository: { full_name: repository } } as never;
		},
		recent: async () => [7],
		artifacts: async (repository: string, run: number) => {
			asked.push(`github artifacts ${repository} ${run}`);
			return { commit: 'abc', artifacts: [GITHUB], built: ['console'] };
		},
		download: async (repository: string, run: number, artifact: Artifact) => {
			asked.push(`github download ${repository} ${run} ${artifact.id}`);
		},
	};
	return { bucket: new Bucket(ACCESS, github, fetch), asked };
}

describe('a request to the bucket', () => {
	// AWS's own ListObjects example, on its own host, signs to 34b48302...dc6f7, as this did; the
	// host is a reserved one here, so the value is this function's, held against change.
	it("is signed as AWS signs its own example, on a host of the test's", () => {
		const access = {
			endpoint: 'https://s3.example.com',
			region: 'us-east-1',
			bucket: 'examplebucket',
			keyId: 'AKIAIOSFODNN7EXAMPLE',
			secret: 'wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY',
			virtualHost: true,
		};
		const at = new Date('2013-05-24T00:00:00Z');
		const { url, headers } = signed(access, '', { 'max-keys': '2', prefix: 'J' }, at);
		expect(url).toBe('https://examplebucket.s3.example.com/?max-keys=2&prefix=J');
		expect(headers.authorization).toBe(
			'AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, ' +
				'SignedHeaders=host;x-amz-content-sha256;x-amz-date, ' +
				'Signature=d1a5045b1a53a3368e24ce87948218fc76eee333e105a239fe8304bd4b36b1b3',
		);
	});

	it('names the bucket in the path unless told to name it in the host', () => {
		const at = new Date('2026-10-13T00:00:00Z');
		const key = `${PLATFORM}/7/worker-console.zip`;
		expect(signed(ACCESS, key, {}, at).url).toBe(
			'http://localhost:9000/artifacts/monoflake/platform/7/worker-console.zip',
		);
		const virtual = { ...ACCESS, endpoint: 'https://s3.example.com', virtualHost: true };
		expect(signed(virtual, key, {}, at).url).toBe(
			'https://artifacts.s3.example.com/monoflake/platform/7/worker-console.zip',
		);
	});
});

describe('what the bucket answers', () => {
	it('reads a page of a listing, and the token for the next where it is cut', () => {
		const xml =
			'<ListBucketResult><IsTruncated>true</IsTruncated>' +
			'<Contents><Key>a/b&amp;c.zip</Key><Size>12</Size></Contents>' +
			'<NextContinuationToken>n&amp;1</NextContinuationToken></ListBucketResult>';
		const page = pageOf(xml);
		expect([...page.objects]).toEqual([['a/b&c.zip', 12]]);
		expect(page.next).toBe('n&1');
		expect(pageOf('<ListBucketResult></ListBucketResult>').next).toBeUndefined();
	});

	it('takes a record only where it names a SHA-256 and a size', () => {
		const hex = sha256('x');
		expect(recordOf({ sha256: hex.toUpperCase(), size: 1 })).toEqual({
			digest: `sha256:${hex}`,
			size: 1,
		});
		expect(recordOf({ sha256: 'abc', size: 1 })).toBeUndefined();
		expect(recordOf({ sha256: hex, size: '1' })).toBeUndefined();
		expect(recordOf(null)).toBeUndefined();
	});
});

describe('the bucket', () => {
	it("lists a run's Workers, images and declarations, each with what its record says", async () => {
		const objects = new Map([
			...uploaded('worker-console', 'console'),
			...uploaded('deploy-geo-arm64', 'geo'),
			...uploaded('deploy-geo-amd64', 'geo'),
			...uploaded(DECLARATIONS, 'declared'),
		]);
		const { bucket } = bucketOf(objects);
		expect(await bucket.artifacts(PLATFORM, 7)).toEqual({
			commit: 'abc',
			artifacts: [{ app: 'console', digest: `sha256:${sha256('console')}`, size: 7 }],
			built: ['console', 'geo'],
			images: ['geo'],
			declarations: { app: DECLARATIONS, digest: `sha256:${sha256('declared')}`, size: 8 },
		});
	});

	it('uses no artifact without its record, nor a Worker its record does not match', async () => {
		const objects = new Map([
			[`${PLATFORM}/7/worker-console.zip`, 'console'],
			...uploaded('worker-aka', 'aka', { size: 99 }),
			...uploaded('worker-cdn', 'cdn', { sha256: 'nope' }),
			[`${PLATFORM}/7/deploy-geo-arm64.zip`, 'geo'],
		]);
		const { bucket } = bucketOf(objects);
		const built = await bucket.artifacts(PLATFORM, 7);
		expect(built.artifacts).toEqual([]);
		expect(built.built).toEqual(['aka', 'cdn']);
		expect(built.images).toEqual([]);
		expect(built.declarations).toBeUndefined();
	});

	it('reads nothing past the listing of a run GitHub refused', async () => {
		const objects = new Map(uploaded('worker-console', 'console'));
		const { bucket, asked } = bucketOf(objects, undefined, () => {
			throw new Refused('run 7 is not one to deploy: it ran .github/workflows/ci.yml');
		});
		await expect(bucket.artifacts(PLATFORM, 7)).rejects.toBeInstanceOf(Refused);
		expect(asked).toEqual([`list ${PLATFORM}/7/`]);
	});

	it('leaves a run it holds nothing of to GitHub, its listing and its download', async () => {
		const objects = new Map(uploaded('worker-console', 'console'));
		const { bucket, asked } = bucketOf(objects);
		const built = await bucket.artifacts('canmi21/web', 7);
		expect(built.artifacts).toEqual([GITHUB]);
		const zip = join(await mkdtemp(join(tmpdir(), 'deployer-')), 'artifact.zip');
		await bucket.download('canmi21/web', 7, GITHUB, zip);
		expect(asked).toEqual([
			'list canmi21/web/7/',
			'github artifacts canmi21/web 7',
			'github download canmi21/web 7 1',
		]);
	});

	it('follows a listing over its pages', async () => {
		let page = 0;
		const objects = new Map(uploaded('worker-console', 'console'));
		const { bucket } = bucketOf(objects, (key) => {
			if (key !== '' || page > 1) return undefined;
			page += 1;
			const [name, data] = [...objects][page - 1]!;
			const contents = `<Contents><Key>${name}</Key><Size>${Buffer.byteLength(data)}</Size></Contents>`;
			const more =
				page === 1
					? '<IsTruncated>true</IsTruncated><NextContinuationToken>t</NextContinuationToken>'
					: '';
			return new Response(`<ListBucketResult>${more}${contents}</ListBucketResult>`);
		});
		expect((await bucket.artifacts(PLATFORM, 7)).artifacts.map((artifact) => artifact.app)).toEqual(
			['console'],
		);
	});

	it('downloads a Worker held to its digest and size', async () => {
		const objects = new Map(uploaded('worker-console', 'console'));
		const { bucket } = bucketOf(objects);
		const [artifact] = (await bucket.artifacts(PLATFORM, 7)).artifacts;
		const zip = join(await mkdtemp(join(tmpdir(), 'deployer-')), 'artifact.zip');
		await bucket.download(PLATFORM, 7, artifact!, zip);
		expect(await readFile(zip, 'utf8')).toBe('console');

		objects.set(`${PLATFORM}/7/worker-console.zip`, 'replaced');
		await expect(bucket.download(PLATFORM, 7, artifact!, zip)).rejects.toThrow(
			new Refused('artifact worker-console does not match its recorded digest'),
		);
	});

	const CONSOLE: Artifact = { app: 'console', digest: `sha256:${sha256('console')}`, size: 7 };
	const OF = 'artifact worker-console of run 7 of monoflake/platform';

	/** What downloading the console threw, the bucket answering `answer` for it. */
	async function thrown(answer: (key: string) => Response | undefined) {
		const { bucket } = bucketOf(new Map(uploaded('worker-console', 'console')), answer);
		const zip = join(await mkdtemp(join(tmpdir(), 'deployer-')), 'artifact.zip');
		return bucket.download(PLATFORM, 7, CONSOLE, zip).then(
			() => undefined,
			(error: unknown) => error,
		);
	}

	it('says an artifact deleted is no longer kept, for good', async () => {
		const error = await thrown(() => new Response(null, { status: 404 }));
		expect(error).toBeInstanceOf(Unread);
		expect(error).not.toBeInstanceOf(Transient);
		expect(error).toHaveProperty('message', `${OF} is no longer kept in the bucket`);
	});

	it('says a bucket that would not serve is a reason of the moment', async () => {
		const slow = '<Error><Code>SlowDown</Code></Error>';
		const error = await thrown(() => new Response(slow, { status: 503 }));
		expect(error).toBeInstanceOf(Transient);
		expect(error).toHaveProperty('message', `the bucket answered 503 for ${OF}: SlowDown`);
	});

	it('says a bucket it could not reach is a reason of the moment', async () => {
		const error = await thrown(() => {
			throw new TypeError('fetch failed', { cause: new Error('connect ECONNREFUSED') });
		});
		expect(error).toBeInstanceOf(Transient);
		expect(error).toHaveProperty(
			'message',
			`the bucket could not be reached for ${OF}: fetch failed: connect ECONNREFUSED`,
		);
	});

	it('names the key a refusal is about, never the key itself', async () => {
		const denied = '<Error><Code>AccessDenied</Code></Error>';
		const error = await thrown(() => new Response(denied, { status: 403 }));
		expect(error).not.toBeInstanceOf(Transient);
		expect(error).toHaveProperty(
			'message',
			`the bucket answered 403 for ${OF}: AccessDenied; ARTIFACTS_READ_KEY_ID may not read it`,
		);
	});
});

describe('the bucket configured', () => {
	const ALL = {
		ARTIFACTS_ENDPOINT: 'http://localhost:9000',
		ARTIFACTS_REGION: 'us-east-1',
		ARTIFACTS_BUCKET: 'artifacts',
		ARTIFACTS_READ_KEY_ID: 'reader',
		ARTIFACTS_READ_SECRET: 'secret',
	};

	it('is read when all of it is set, and GitHub otherwise', () => {
		expect(configOf(ALL).artifacts).toEqual(ACCESS);
		expect(configOf({ ...ALL, ARTIFACTS_VIRTUAL_HOST: '1' }).artifacts?.virtualHost).toBe(true);
		expect(configOf({ ...ALL, ARTIFACTS_READ_SECRET: ' ' }).artifacts).toBeUndefined();
		expect(configOf({}).artifacts).toBeUndefined();
	});

	it('names what is missing where only part of it is set', () => {
		expect(bucketMissing({})).toEqual([]);
		expect(bucketMissing(ALL)).toEqual([]);
		expect(bucketMissing({ ARTIFACTS_BUCKET: 'artifacts' })).toEqual([
			'ARTIFACTS_ENDPOINT',
			'ARTIFACTS_REGION',
			'ARTIFACTS_READ_KEY_ID',
			'ARTIFACTS_READ_SECRET',
		]);
	});
});

describe('a Worker read from the bucket', () => {
	it('deploys, and is taken again by catching up when the bucket would not serve it', async () => {
		const data = await mkdtemp(join(tmpdir(), 'deployer-'));
		const config = configOf({
			DEPLOYER_DATA: data,
			DEPLOY_SOURCES: PLATFORM,
			WORKER_OWNERS: `console=${PLATFORM}`,
			WORKER_ZONES: `${PLATFORM}=canmi.app`,
			CLOUDFLARE_ACCOUNT_ID: 'account',
			CLOUDFLARE_WORKERS_TOKEN: 'token',
		});
		const declaration = {
			name: 'console',
			main: 'bundle/_worker.js',
			no_bundle: true,
			compatibility_date: '2026-07-29',
		};
		const rest = PACKAGED.filter((item) => item.name !== 'wrangler.json');
		const zip = zipOf([{ name: 'wrangler.json', data: JSON.stringify(declaration) }, ...rest]);
		const record = { sha256: createHash('sha256').update(zip).digest('hex'), size: zip.length };
		const objects = new Map<string, Buffer>();
		for (const run of [5, 7]) {
			objects.set(`${PLATFORM}/${run}/worker-console.zip`, zip);
			objects.set(`${PLATFORM}/${run}/worker-console.json`, Buffer.from(JSON.stringify(record)));
		}
		let unavailable = true;
		const fetch = async (input: string) => {
			const url = new URL(input);
			if (url.searchParams.get('list-type') === '2') {
				const prefix = url.searchParams.get('prefix') ?? '';
				const contents = [...objects]
					.filter(([key]) => key.startsWith(prefix))
					.map(
						([key, body]) => `<Contents><Key>${key}</Key><Size>${body.length}</Size></Contents>`,
					);
				return new Response(`<ListBucketResult>${contents.join('')}</ListBucketResult>`);
			}
			const key = decodeURIComponent(url.pathname.replace(/^\/artifacts\//, ''));
			if (key === `${PLATFORM}/7/worker-console.zip` && unavailable) {
				return new Response(null, { status: 503 });
			}
			const body = objects.get(key);
			return body ? new Response(new Uint8Array(body)) : new Response(null, { status: 404 });
		};
		const github = {
			run: async () => ({ ...RECORD }) as never,
			recent: async () => [5, 7],
			artifacts: async () => {
				throw new Error('the bucket holds this run');
			},
			download: async () => {
				throw new Error('the bucket holds this run');
			},
		};
		const bucket = new Bucket(ACCESS, github, fetch);
		const store = new Store(join(data, 'deployer.db'));
		const logs: string[] = [];
		const deployer = new Deployer({
			config,
			store,
			github: bucket,
			wrangler: async () => ({ code: 0, output: 'Current Version ID: v' }),
			log: (line) => logs.push(line),
		});
		const catchUp = new CatchUp({ sources: config.sources, store, github: bucket, deployer });
		for (const run of [5, 7]) deployer.notice({ run, repository: PLATFORM });
		await deployer.idle();
		expect(store.list().map((row) => [row.run, row.stage])).toEqual([
			[7, 'failed'],
			[5, 'deployed'],
		]);
		expect(store.list()[0]!.error).toBe(
			`the bucket answered 503 for artifact worker-console of run 7 of ${PLATFORM}`,
		);

		unavailable = false;
		await catchUp.pass();
		await deployer.idle();
		expect(store.list()[0]).toMatchObject({ run: 7, worker: 'console', stage: 'deployed' });
		expect(store.lastRun('console')).toBe(7);
	});
});
