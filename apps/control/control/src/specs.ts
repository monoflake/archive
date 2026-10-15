/**
 * What each kind's spec has to be before control stores it, as the old registry checked it: a
 * node's record, whose other keys are kept as given; an app's spec and an account's, which hold
 * nothing else. A node has no name and is shown by its city, country and continent, and an app's
 * placements are node ids. See RFC o7eir and spec/architecture/control.md.
 */
import type { Code } from '@canmi/response';

export const ID = /^[0-9a-f]{16}$/;
export const LABEL = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

/** 16 lowercase hexadecimal digits, as every resource's id is. */
export function isId(key: string): boolean {
	return ID.test(key);
}

/** One DNS label, and never an id's shape, which a route would read as an id. */
export function isLabel(name: string): boolean {
	return LABEL.test(name) && !isId(name);
}

export const TIERS = ['datacenter', 'home', 'transient'];
export const SYSTEMS = ['debian', 'alpine'];
/** A provider's machine, or one in the author's own hands -- RFC 32hd9. */
export const HOSTINGS = ['datacenter', 'selfhost'];
/** What a node holds: functions and services, data, the platform's database and schedules. */
export const ROLES = ['compute', 'storage', 'core'];
/** Each continent by its two letters -- infra's spec/architecture/nodes.md. */
export const CONTINENTS = ['AF', 'AN', 'AS', 'EU', 'NA', 'OC', 'SA'];

/** Whose account it is, each by a whole word, never a short code. */
export const PROVIDERS = ['oracle', 'azure', 'racknerd', 'backblaze', 'cloudflare'];
/**
 * What an identity is of, in words no one provider owns: the account's id at its provider, the
 * name its user signs in with, and its email.
 */
export const IDENTITIES = ['account_id', 'username', 'email'];

/** Every key an app's spec may hold. */
const APP_FIELDS = new Set(['placements', 'release', 'display_name', 'repository']);
/** Every key a `release` holds, each required. */
const RELEASE = new Set(['repository', 'run', 'commit']);
/** Every key an account's spec may hold. */
const ACCOUNT_FIELDS = new Set(['provider', 'identities']);
/** Every key an identity holds, each required. */
const IDENTITY = new Set(['kind', 'value']);
/** The longest identity kept: an email or a provider's id is far shorter. */
const MAX_VALUE_BYTES = 1024;

export type Spec = Record<string, unknown>;

/** Why a write is not taken, answered in the envelope. */
export interface Refusal {
	status: number;
	code: Code;
	message: string;
}

const invalid = (message: string): Refusal => ({ status: 400, code: 'invalid_body', message });
const forbidden = (message: string): Refusal => ({ status: 400, code: 'forbidden_field', message });

export function isObject(value: unknown): value is Spec {
	return typeof value === 'object' && value !== null && !Array.isArray(value);
}

const OCTET = '(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])';
const IPV4 = new RegExp(`^${OCTET}(\\.${OCTET}){3}$`);

/** A node's record as stored, or what is wrong with it, said for the operator who sent it. */
export function node(spec: unknown): Refusal | undefined {
	if (!isObject(spec)) return invalid('a node is a JSON object');
	const text = (key: string): string | Refusal => {
		const value = spec[key];
		if (typeof value === 'string') return value;
		return invalid(value === undefined ? `\`${key}\` is required` : `\`${key}\` is a string`);
	};
	const address = text('address');
	if (typeof address !== 'string') return address;
	if (!IPV4.test(address)) return invalid("`address` is the node's tailnet IPv4, a dotted quad");
	const tier = text('tier');
	if (typeof tier !== 'string') return tier;
	if (!TIERS.includes(tier)) return invalid(`\`tier\` is one of ${TIERS.join(', ')}`);
	// Read from its account, never written -- RFC 32hd9.
	if ('domain' in spec) {
		return invalid("`domain` is not written: a node's failure domain is its account");
	}
	const system = text('system');
	if (typeof system !== 'string') return system;
	if (!SYSTEMS.includes(system)) return invalid(`\`system\` is one of ${SYSTEMS.join(', ')}`);
	if ('core' in spec && typeof spec.core !== 'boolean') return invalid('`core` is true or false');
	// A node leaves by this mark, never by its record going missing -- RFC qcyb3.
	if ('retired' in spec && typeof spec.retired !== 'boolean') {
		return invalid('`retired` is true or false');
	}
	if ('expiry' in spec && !isYear(spec.expiry)) {
		return invalid('`expiry` is a year, as a whole number');
	}
	if ('hosting' in spec && !HOSTINGS.includes(spec.hosting as string)) {
		return invalid(`\`hosting\` is one of ${HOSTINGS.join(', ')}`);
	}
	if ('account' in spec && !(typeof spec.account === 'string' && isId(spec.account))) {
		return invalid('`account` is the id of the account the node is under');
	}
	if ('roles' in spec && !onceEach(spec.roles, ROLES)) {
		return invalid(`\`roles\` is a list of one or more of ${ROLES.join(', ')}, each once`);
	}
	// Where the node is, which the console draws and orders nodes by.
	if ('coordinates' in spec && !areCoordinates(spec.coordinates)) {
		return invalid('`coordinates` is [latitude, longitude] in degrees');
	}
	if ('place' in spec) {
		return invalid('`place` is not written: a node is shown by its city, country and continent');
	}
	return shown(text);
}

/** What control shows a node as -- infra's spec/architecture/nodes.md. */
function shown(text: (key: string) => string | Refusal): Refusal | undefined {
	const city = text('city');
	if (typeof city !== 'string') return city;
	if (!isDisplay(city)) return invalid('`city` is a name of 1 to 64 characters');
	const country = text('country');
	if (typeof country !== 'string') return country;
	if (!/^[A-Z]{2}$/.test(country)) return invalid("`country` is ISO 3166-1's two capital letters");
	const continent = text('continent');
	if (typeof continent !== 'string') return continent;
	if (!CONTINENTS.includes(continent)) {
		return invalid(`\`continent\` is one of ${CONTINENTS.join(', ')}`);
	}
	return undefined;
}

function isYear(year: unknown): boolean {
	return Number.isInteger(year) && (year as number) >= 1000 && (year as number) <= 9999;
}

/** A list of one or more of `choices`, each once. */
function onceEach(list: unknown, choices: readonly string[]): boolean {
	if (!Array.isArray(list) || list.length === 0) return false;
	return list.every(
		(item, index) => choices.includes(item as string) && list.indexOf(item) === index,
	);
}

/** `[latitude, longitude]`, each a number in degrees within its range. */
function areCoordinates(coordinates: unknown): boolean {
	if (!Array.isArray(coordinates) || coordinates.length !== 2) return false;
	const [latitude, longitude] = coordinates as unknown[];
	return (
		typeof latitude === 'number' &&
		typeof longitude === 'number' &&
		Math.abs(latitude) <= 90 &&
		Math.abs(longitude) <= 180
	);
}

/** A display name: not empty, not padded, at most 64 characters, as a `service.toml`'s is. */
function isDisplay(name: string): boolean {
	return name !== '' && name.trim() === name && [...name].length <= 64;
}

/**
 * An app's spec as stored: `placements`, the ids of the nodes it runs on, each once and in the
 * order given, none for a Worker; `release`, what it runs there, absent until one is recorded --
 * RFC jln34; and from its declaration, `display_name`, what a person reads it as, and
 * `repository`, the `owner/repo` it is declared in -- RFC 17b3o. Which nodes control holds is
 * checked apart from this, against the database.
 */
export function app(spec: unknown): Refusal | undefined {
	if (!isObject(spec)) return invalid("an app's spec is a JSON object");
	const other = Object.keys(spec).find((key) => !APP_FIELDS.has(key));
	if (other !== undefined) return forbidden(`\`${other}\` is not a field of an app`);
	const release = 'release' in spec ? releaseRefusal(spec.release) : undefined;
	if (release !== undefined) return invalid(release);
	if (
		'display_name' in spec &&
		!(typeof spec.display_name === 'string' && isDisplay(spec.display_name))
	) {
		return invalid('`display_name` is a name of 1 to 64 characters');
	}
	if ('repository' in spec && !isRepository(spec.repository)) {
		return invalid('`repository` is a GitHub repository, as `owner/repo`');
	}
	const { placements } = spec;
	if (placements === undefined) return invalid('`placements` is required');
	if (!Array.isArray(placements)) return invalid('`placements` is a list of node ids');
	for (const [index, placement] of placements.entries()) {
		if (typeof placement !== 'string' || !isId(placement)) {
			return invalid(`\`placements\` holds ${JSON.stringify(placement)}, not a node id`);
		}
		if (placements.indexOf(placement) !== index) {
			return invalid(`\`placements\` names ${placement} twice`);
		}
	}
	return undefined;
}

/** What is wrong with a `release`, if anything: the run that built it, and the commit it built. */
function releaseRefusal(release: unknown): string | undefined {
	if (!isObject(release)) return '`release` is an object of `repository`, `run` and `commit`';
	const other = Object.keys(release).find((key) => !RELEASE.has(key));
	if (other !== undefined) return `\`release\` has no \`${other}\``;
	const missing = [...RELEASE].find((key) => !(key in release));
	if (missing !== undefined) return `\`release.${missing}\` is required`;
	if (!isRepository(release.repository)) {
		return '`release.repository` is a GitHub repository, as `owner/repo`';
	}
	if (!(Number.isSafeInteger(release.run) && (release.run as number) > 0)) {
		return "`release.run` is a run's number, a whole number above 0";
	}
	if (!(typeof release.commit === 'string' && /^[0-9a-f]{40}$/.test(release.commit))) {
		return "`release.commit` is a commit's 40 lowercase hexadecimal digits";
	}
	return undefined;
}

/** `owner/repo`, each as GitHub allows: letters, digits, `-`, `_` and `.`. */
function isRepository(repository: unknown): boolean {
	return typeof repository === 'string' && /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository);
}

/**
 * An account's spec as stored: `provider`, one of `PROVIDERS`, and `identities`, one to three,
 * each `{kind, value}` and each kind at most once, so an account always keeps an identifier. That
 * no other account at its provider holds one of them is checked apart from this.
 */
export function account(spec: unknown): Refusal | undefined {
	if (!isObject(spec)) return invalid("an account's spec is a JSON object");
	const other = Object.keys(spec).find((key) => !ACCOUNT_FIELDS.has(key));
	if (other !== undefined) return forbidden(`\`${other}\` is not a field of an account`);
	if (!PROVIDERS.includes(spec.provider as string)) {
		return invalid(`\`provider\` is one of ${PROVIDERS.join(', ')}`);
	}
	const held = spec.identities;
	if (!Array.isArray(held)) {
		return invalid('`identities` is required, a list of one to three `{kind, value}`');
	}
	if (held.length === 0 || held.length > IDENTITIES.length) {
		return invalid('`identities` holds one to three, so an account always keeps an identifier');
	}
	for (const identity of held) {
		const refusal = identityRefusal(identity);
		if (refusal !== undefined) return refusal;
	}
	const kinds = held.map((identity: Spec) => identity.kind);
	const twice = kinds.find((kind, index) => kinds.indexOf(kind) !== index);
	if (twice !== undefined) {
		return invalid(`\`identities\` names the kind "${twice}" twice; each is held once`);
	}
	return undefined;
}

function identityRefusal(identity: unknown): Refusal | undefined {
	if (!isObject(identity)) return invalid('an identity is an object of `kind` and `value`');
	const other = Object.keys(identity).find((key) => !IDENTITY.has(key));
	if (other !== undefined) {
		return forbidden(`an identity holds \`kind\` and \`value\` alone, never \`${other}\``);
	}
	if (!IDENTITIES.includes(identity.kind as string)) {
		return invalid(`an identity's \`kind\` is one of ${IDENTITIES.join(', ')}`);
	}
	const { value } = identity;
	if (
		typeof value !== 'string' ||
		value === '' ||
		new TextEncoder().encode(value).length > MAX_VALUE_BYTES ||
		/[\n\r\0]/.test(value)
	) {
		return invalid("an identity's `value` is required, one line of text, at most 1 KiB");
	}
	return undefined;
}

/** An identity as two are compared: an email in lower case, any other exactly. */
export interface Identity {
	provider: string;
	kind: string;
	value: string;
}

/** The identities of an account's spec already checked, as they are compared. */
export function identities(spec: Spec): Identity[] {
	const provider = spec.provider as string;
	return (spec.identities as { kind: string; value: string }[]).map(({ kind, value }) => ({
		provider,
		kind,
		value: kind === 'email' ? value.toLowerCase() : value,
	}));
}
