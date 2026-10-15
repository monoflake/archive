/**
 * Everything the deployer is told, read from its environment: `config.env` and `secret.env` on its
 * node. The sources, the owners and the zones are the operator's switches, never the artifact's.
 * See spec/architecture/deployer.md, "What it refuses" and "Admitting a repository".
 */

/** Its declared port; see spec/architecture/services.md, "A service keeps one port". */
export const PORT = 12020;

export interface Config {
	/** The repositories whose runs it deploys, from `DEPLOY_SOURCES`, as host reads its own. */
	readonly sources: readonly string[];
	/** Each Worker's name against the `owner/repo` whose runs alone deploy it: `WORKER_OWNERS`. */
	readonly owners: ReadonlyMap<string, string>;
	/** The zones each `owner/repo`'s routes and custom domains may be on: `WORKER_ZONES`. */
	readonly zones: ReadonlyMap<string, readonly string[]>;
	/**
	 * The account resources each `owner/repo` outside the platform may bind: `WORKER_RESOURCES`,
	 * `canmi21/web=d1:<database_id> r2:<bucket_name> service:<worker>, other/repo=...`, each a kind
	 * and the binding's identifying field as bindings.ts reads them.
	 */
	readonly resources: ReadonlyMap<string, readonly string[]>;
	/** Every deploy runs `--dry-run`, recorded and deploying nothing: `DEPLOYER_DRY=1`. */
	readonly dry: boolean;
	/** The Workers that run dry while the rest deploy: `DRY_WORKERS`, a comma list. */
	readonly dryWorkers: ReadonlySet<string>;
	/** What a rollback asks for, as host's `HOST_TOKEN`: `DEPLOYER_TOKEN`; none refuses every one. */
	readonly token: string | undefined;
	/** What `GET /api/*` asks for, as host's `HOST_READ_TOKEN`; none refuses every read. */
	readonly readToken: string | undefined;
	/** The default account and its token, `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_WORKERS_TOKEN`. */
	readonly cloudflare: Account;
	/**
	 * The Workers that deploy to another account, by that account's name: `WORKER_ACCOUNTS`,
	 * `console=monoflake`. A Worker not named goes to the default.
	 */
	readonly workerAccounts: ReadonlyMap<string, string>;
	/**
	 * Each account `WORKER_ACCOUNTS` names: its id from `CLOUDFLARE_ACCOUNT_ID_<NAME>`, and its token
	 * from `CLOUDFLARE_WORKERS_TOKEN_<NAME>`, or else the default's, since one user token can cover
	 * every account.
	 */
	readonly accounts: ReadonlyMap<string, Account>;
	/**
	 * Where each app's release is written: `CONTROL`, control's base URL. See releases.ts and
	 * spec/architecture/control.md, "Who reads it, and with what".
	 */
	readonly control: string | undefined;
	/** What a release is written with: `CONTROL_WRITE_TOKEN`; none writes no release. */
	readonly controlToken: string | undefined;
	/**
	 * The bucket a run's artifacts are read from, RFC q28g5: `ARTIFACTS_ENDPOINT`, `_REGION` and
	 * `_BUCKET`, read with `ARTIFACTS_READ_KEY_ID` and `_SECRET`. Unless all five are set, they are
	 * read from GitHub, as before.
	 */
	readonly artifacts: BucketAccess | undefined;
	readonly data: string;
	readonly port: number;
}

/** A Cloudflare account and the token that deploys Workers to it. */
export interface Account {
	readonly account: string | undefined;
	readonly token: string | undefined;
}

/** How the bucket is reached, by the S3 API alone; see bucket.ts. */
export interface BucketAccess {
	readonly endpoint: string;
	readonly region: string;
	readonly bucket: string;
	readonly keyId: string;
	readonly secret: string;
	/** `<bucket>.<endpoint>` rather than `<endpoint>/<bucket>`: `ARTIFACTS_VIRTUAL_HOST=1`. */
	readonly virtualHost: boolean;
}

export type Env = Readonly<Record<string, string | undefined>>;

/** What the bucket is reached with, each required. */
export const BUCKET_VARIABLES = [
	'ARTIFACTS_ENDPOINT',
	'ARTIFACTS_REGION',
	'ARTIFACTS_BUCKET',
	'ARTIFACTS_READ_KEY_ID',
	'ARTIFACTS_READ_SECRET',
] as const;

/** Of `BUCKET_VARIABLES`, those unset where some are set: a bucket configured in part. */
export function bucketMissing(env: Env): string[] {
	const missing = BUCKET_VARIABLES.filter((key) => value(env, key) === undefined);
	return missing.length === BUCKET_VARIABLES.length ? [] : missing;
}

function bucketOf(env: Env): BucketAccess | undefined {
	const [endpoint, region, bucket, keyId, secret] = BUCKET_VARIABLES.map((key) => value(env, key));
	if (!endpoint || !region || !bucket || !keyId || !secret) return undefined;
	const virtualHost = ['1', 'true'].includes(value(env, 'ARTIFACTS_VIRTUAL_HOST') ?? '');
	return { endpoint, region, bucket, keyId, secret, virtualHost };
}

function value(env: Env, key: string): string | undefined {
	const found = env[key]?.trim();
	return found ? found : undefined;
}

/** `owner/name` pairs separated by whitespace; anything else is not a repository. */
export function sources(text: string): string[] {
	return text.split(/\s+/).filter((source) => /^[\w.-]+\/[\w.-]+$/.test(source));
}

/** `key=value` pairs separated by commas, each side trimmed; a pair missing either is dropped. */
export function pairs(text: string): [string, string][] {
	return text.split(',').flatMap((pair) => {
		const at = pair.indexOf('=');
		const key = pair.slice(0, at).trim();
		const rest = pair.slice(at + 1).trim();
		return at > 0 && key && rest ? [[key, rest] as [string, string]] : [];
	});
}

/** The variables an account named `name` is read from: its id's and its token's. */
export function accountVariables(name: string): [string, string] {
	const suffix = name.toUpperCase().replaceAll('-', '_');
	return [`CLOUDFLARE_ACCOUNT_ID_${suffix}`, `CLOUDFLARE_WORKERS_TOKEN_${suffix}`];
}

export function configOf(env: Env): Config {
	const dryWorkers = (value(env, 'DRY_WORKERS') ?? '').split(',').map((name) => name.trim());
	const workerAccounts = new Map(
		pairs(value(env, 'WORKER_ACCOUNTS') ?? '').filter(([, name]) => /^[a-z0-9-]+$/.test(name)),
	);
	const cloudflare = {
		account: value(env, 'CLOUDFLARE_ACCOUNT_ID'),
		token: value(env, 'CLOUDFLARE_WORKERS_TOKEN'),
	};
	const accounts = new Map(
		[...new Set(workerAccounts.values())].map((name) => {
			const [account, token] = accountVariables(name).map((key) => value(env, key));
			return [name, { account, token: token ?? cloudflare.token }] as const;
		}),
	);
	return {
		sources: sources(value(env, 'DEPLOY_SOURCES') ?? ''),
		owners: new Map(pairs(value(env, 'WORKER_OWNERS') ?? '')),
		zones: new Map(
			pairs(value(env, 'WORKER_ZONES') ?? '').map(([owner, zones]) => [owner, zones.split(/\s+/)]),
		),
		resources: new Map(
			pairs(value(env, 'WORKER_RESOURCES') ?? '').map(([owner, list]) => [
				owner,
				list.split(/\s+/),
			]),
		),
		dry: value(env, 'DEPLOYER_DRY') === '1',
		dryWorkers: new Set(dryWorkers.filter(Boolean)),
		token: value(env, 'DEPLOYER_TOKEN'),
		readToken: value(env, 'DEPLOYER_READ_TOKEN'),
		cloudflare,
		workerAccounts,
		accounts,
		control: value(env, 'CONTROL'),
		controlToken: value(env, 'CONTROL_WRITE_TOKEN'),
		artifacts: bucketOf(env),
		data: value(env, 'DEPLOYER_DATA') ?? '/data',
		port: Number(value(env, 'PORT') ?? PORT),
	};
}

/**
 * The account `worker` deploys to and its credentials: the one `WORKER_ACCOUNTS` names, or the
 * default. `missing` names each variable its credentials need and the environment lacks.
 */
export function accountOf(
	config: Config,
	worker: string,
): { name: string | undefined; cloudflare: Account; missing: string[] } {
	const name = config.workerAccounts.get(worker);
	const cloudflare =
		name === undefined
			? config.cloudflare
			: (config.accounts.get(name) ?? { account: undefined, token: undefined });
	const missing = [
		...(cloudflare.account ? [] : [name ? accountVariables(name)[0] : 'CLOUDFLARE_ACCOUNT_ID']),
		...(cloudflare.token ? [] : ['CLOUDFLARE_WORKERS_TOKEN']),
	];
	return { name, cloudflare, missing };
}

/** Whether `worker` runs dry: everything does under `DEPLOYER_DRY`, or the names listed. */
export function isDry(config: Config, worker: string): boolean {
	return config.dry || config.dryWorkers.has(worker);
}

/**
 * The platform's organization: its runs are fetched with `GITHUB_ACTIONS_TOKEN` itself, and its
 * repositories bind any resource, since the platform is the account.
 */
export const HOME_OWNER = 'monoflake';

/**
 * The variable a repository's artifacts are fetched with the token in, picked by its owner: the
 * monoflake organization's own, or that owner's `GITHUB_ACTIONS_TOKEN_<OWNER>` -- `_CANMI21` for
 * `canmi21/web`. See spec/architecture/deployer.md, "Credentials".
 */
export function tokenVariable(repository: string): string | undefined {
	const [owner = ''] = repository.split('/');
	if (owner === HOME_OWNER) return 'GITHUB_ACTIONS_TOKEN';
	if (!/^[\w-]+$/.test(owner)) return undefined;
	return `GITHUB_ACTIONS_TOKEN_${owner.toUpperCase().replaceAll('-', '_')}`;
}

/** The token a repository's artifacts are fetched with, from its `tokenVariable`. */
export function tokenFor(repository: string, env: Env): string | undefined {
	const variable = tokenVariable(repository);
	return variable === undefined ? undefined : value(env, variable);
}
