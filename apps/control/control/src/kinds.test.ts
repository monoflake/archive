import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import * as v from 'valibot';
import { describe, expect, it } from 'vitest';
import { Catalog, type Description, type Field } from './catalog.gen.ts';
import { ACCOUNT, APP, CATALOG, NODE } from './kinds.ts';
import { account, app, node, type Spec } from './specs.ts';
import { control, READ, refused, SIN, TYO } from './testing.ts';

const COMMITTED = join(import.meta.dirname, '../catalog.json');
const EXAMPLES = join(import.meta.dirname, '../examples/catalog');
const ANSWER = join(EXAMPLES, 'valid/answer.json');

/** Whether `kind`'s own checks store `spec`; what it names in the database is checked apart. */
function accepts(kind: Description, spec: unknown): boolean {
	return { node, app, account }[kind.name](spec) === undefined;
}

/** A spec each kind stores, every catalog field in it; a node's keeps two keys more. */
function example(kind: Description): Spec {
	switch (kind.name) {
		case 'node':
			return {
				city: 'Tokyo',
				country: 'JP',
				continent: 'AS',
				address: '100.64.0.9',
				tier: 'datacenter',
				system: 'debian',
				core: true,
				expiry: 2036,
				retired: false,
				hosting: 'datacenter',
				account: '0123456789abcdef',
				roles: ['compute', 'core'],
				canary: true,
				grants: ['database'],
			};
		case 'app':
			return {
				placements: [TYO, SIN],
				release: {
					repository: 'monoflake/platform',
					run: 7,
					commit: '0123456789abcdef0123456789abcdef01234567',
				},
				display_name: 'Task Ledger',
				repository: 'monoflake/platform',
			};
		case 'account':
			return { provider: 'oracle', identities: [{ kind: 'email', value: 'me@example.com' }] };
	}
}

function wrongTypes(type: Field['type']): unknown[] {
	switch (type) {
		case 'string':
			return [1, true, null, ['x']];
		case 'integer':
			return ['2036', 2036.5, true, null];
		case 'boolean':
			return ['yes', 1, null];
		case 'choice':
			return ['none of them', 1, null];
		case 'list':
			return ['tyo', 1, {}, null];
		case 'object':
			return ['x', 1, [], null];
		case 'reference':
			return [1, null, ['x'], '0123456789ABCDEF'];
	}
}

function each(check: (kind: Description, field: Field) => void) {
	for (const kind of CATALOG.items) for (const field of kind.fields) check(kind, field);
}

describe('the catalog', () => {
	it('is what catalog.schema.json describes', () => {
		expect(v.safeParse(Catalog, CATALOG).issues).toBeUndefined();
	});

	// A reader's tests load it whole rather than keep a copy -- RFC bq3vn.
	it('is what catalog.json holds, which REGENERATE=1 writes again', () => {
		if (process.env.REGENERATE === '1') {
			writeFileSync(COMMITTED, `${JSON.stringify(CATALOG.items, null, '\t')}\n`);
		}
		const committed: unknown = JSON.parse(readFileSync(COMMITTED, 'utf8'));
		expect(committed, 'write it again with `REGENERATE=1 vitest run kinds`').toEqual(CATALOG.items);
	});

	// What `GET /v1/kinds` answers, as readers in other languages test against.
	it('is answered as examples/catalog/valid/answer.json holds, which REGENERATE=1 writes', () => {
		if (process.env.REGENERATE === '1')
			writeFileSync(ANSWER, `${JSON.stringify(CATALOG, null, '\t')}\n`);
		const committed: unknown = JSON.parse(readFileSync(ANSWER, 'utf8'));
		expect(committed, 'write it again with `REGENERATE=1 vitest run kinds`').toEqual(CATALOG);
	});

	it('takes each example under valid/ and refuses each under invalid/', () => {
		const judged: string[] = [];
		for (const [directory, valid] of [
			['valid', true],
			['invalid', false],
		] as const) {
			const files = readdirSync(join(EXAMPLES, directory)).filter((file) => file.endsWith('.json'));
			for (const file of files.toSorted()) {
				const given: unknown = JSON.parse(readFileSync(join(EXAMPLES, directory, file), 'utf8'));
				const named = `${directory}/${file}`;
				expect(v.safeParse(Catalog, given).success, named).toBe(valid);
				judged.push(named);
			}
		}
		expect(judged).toEqual(
			expect.arrayContaining([
				'valid/answer.json',
				'valid/minimal.json',
				'invalid/field-without-type.json',
				'invalid/kind-without-plural.json',
				'invalid/choices-not-strings.json',
				'invalid/secret-not-boolean.json',
				'invalid/kind-null.json',
			]),
		);
	});

	it('names each kind and its plural once, a node and an account with no name', () => {
		expect(CATALOG.items.map((kind) => [kind.name, kind.plural, kind.named])).toEqual([
			['node', 'nodes', false],
			['app', 'apps', true],
			['account', 'accounts', false],
		]);
		for (const kind of CATALOG.items) {
			const names = kind.fields.map((field) => field.name);
			expect(new Set(names).size, kind.name).toBe(names.length);
			for (const column of kind.columns) expect(names, kind.name).toContain(column);
		}
	});

	it('shows a node by its city, country and continent, and places an app on node ids', () => {
		expect(NODE.fields.slice(0, 3).map((field) => [field.name, field.required])).toEqual([
			['city', true],
			['country', true],
			['continent', true],
		]);
		expect(NODE.fields.map((field) => field.name)).not.toContain('place');
		expect(APP.fields[0]).toEqual({
			name: 'placements',
			type: 'list',
			kind: 'node',
			editable: true,
			required: true,
		});
		const identities = ACCOUNT.fields.find((field) => field.name === 'identities');
		expect(identities?.fields?.[1]).toMatchObject({ name: 'value', masked: true });
	});

	it('stores every example, which names every field', () => {
		for (const kind of CATALOG.items) {
			expect(accepts(kind, example(kind)), kind.name).toBe(true);
			for (const field of kind.fields) expect(example(kind), kind.name).toHaveProperty(field.name);
		}
	});

	it('requires a field exactly where it says', () => {
		each((kind, field) => {
			const { [field.name]: _, ...without } = example(kind);
			expect(accepts(kind, without), `${kind.name}: ${field.name}`).toBe(!field.required);
		});
	});

	it('refuses a value not of its type', () => {
		each((kind, field) => {
			for (const value of wrongTypes(field.type)) {
				const spec = { ...example(kind), [field.name]: value };
				expect(accepts(kind, spec), `${kind.name}: ${field.name} = ${value}`).toBe(false);
			}
		});
	});

	it('takes every choice, and a list of them each once and no other', () => {
		each((kind, field) => {
			const listed = (field.choices ?? []).length > 0;
			if (field.type === 'choice') expect(listed, field.name).toBe(true);
			else if (field.type !== 'list') expect(listed, field.name).toBe(false);
			for (const choice of field.choices ?? []) {
				const value = field.type === 'list' ? [choice] : choice;
				expect(accepts(kind, { ...example(kind), [field.name]: value }), choice).toBe(true);
			}
			if (field.type === 'list' && listed) {
				const [first] = field.choices ?? [];
				expect(accepts(kind, { ...example(kind), [field.name]: ['none of them'] })).toBe(false);
				expect(accepts(kind, { ...example(kind), [field.name]: [first, first] })).toBe(false);
			}
		});
	});

	it('keeps a key it does not name on a node, and refuses one on every other kind', () => {
		for (const kind of CATALOG.items) {
			for (const value of [null, 'x', 1, [], {}]) {
				const spec = { ...example(kind), unnamed: value };
				expect(accepts(kind, spec), `${kind.name}: ${value}`).toBe(kind.name === 'node');
			}
		}
	});

	it('is answered at /v1/kinds to the read token alone', async () => {
		const call = control();
		expect(await call('GET', '/v1/registry/kinds', { token: READ })).toEqual({
			status: 200,
			body: { status: 'success', data: CATALOG },
		});
		expect(await call('GET', '/v1/registry/kinds')).toEqual(refused(401, 'invalid_token'));
		expect(await call('POST', '/v1/registry/kinds', { token: READ })).toEqual(
			refused(405, 'invalid_method'),
		);
	});
});
