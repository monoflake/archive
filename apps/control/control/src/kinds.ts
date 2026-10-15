/**
 * The catalog of kinds, `GET /v1/registry/kinds`: what the console draws a resource from, so a
 * field added here appears there without a change to it -- RFC cpc2b. Typed by the reader made of
 * `catalog.schema.json`, so the compiler holds it to the schema; the tests hold it to what
 * specs.ts accepts and to the committed `catalog.json`. See RFC o7eir.
 */
import type { Catalog, Description, Field, Kind } from './catalog.gen.ts';
import { CONTINENTS, HOSTINGS, IDENTITIES, PROVIDERS, ROLES, SYSTEMS, TIERS } from './specs.ts';

/** A field, `extra` in the place the old registry answered it, ahead of the two every one has. */
function field(name: string, type: Field['type'], required: boolean, extra: Partial<Field> = {}) {
	return { name, type, ...extra, editable: true, required } satisfies Field;
}

function choice(name: string, values: string[], required: boolean): Field {
	return field(name, 'choice', required, { choices: values });
}

/** A list of `choices`, each at most once. */
function choices(name: string, values: string[], required: boolean): Field {
	return field(name, 'list', required, { choices: values });
}

function reference(name: string, kind: Kind, required: boolean): Field {
	return field(name, 'reference', required, { kind });
}

export const NODE: Description = {
	name: 'node',
	plural: 'nodes',
	named: false,
	fields: [
		field('city', 'string', true),
		field('country', 'string', true),
		choice('continent', CONTINENTS, true),
		field('address', 'string', true),
		choice('tier', TIERS, true),
		choice('system', SYSTEMS, true),
		field('core', 'boolean', false),
		field('expiry', 'integer', false),
		field('retired', 'boolean', false),
		choice('hosting', HOSTINGS, false),
		reference('account', 'account', false),
		choices('roles', ROLES, false),
	],
	actions: [],
	columns: ['city', 'country', 'address', 'tier', 'account', 'system'],
};

export const APP: Description = {
	name: 'app',
	plural: 'apps',
	named: true,
	fields: [
		field('placements', 'list', true, { kind: 'node' }),
		field('release', 'object', false),
		field('display_name', 'string', false),
		field('repository', 'string', false),
	],
	actions: [],
	columns: ['display_name', 'placements'],
};

/** An account's identity: its kind, and its value, drawn hidden until asked for. */
const IDENTITY: Field[] = [
	choice('kind', IDENTITIES, true),
	field('value', 'string', true, { masked: true }),
];

export const ACCOUNT: Description = {
	name: 'account',
	plural: 'accounts',
	named: false,
	fields: [
		choice('provider', PROVIDERS, true),
		field('identities', 'list', true, { fields: IDENTITY }),
	],
	actions: [],
	columns: ['provider'],
};

/** What `GET /v1/registry/kinds` answers inside the envelope: every kind control holds. */
export const CATALOG: Catalog = { items: [NODE, APP, ACCOUNT] };
