/**
 * What `PUT /v1/import` reads: every resource the old registry holds, each keeping its id and its
 * revision, and the registry's newest change. Checked whole before anything is written. See RFC
 * o7eir.
 */
import * as v from 'valibot';
import type { Kind, Row } from './store.ts';

const ID = /^[0-9a-f]{16}$/;

/** 16 lowercase hexadecimal digits, as every resource's id is. */
export function isId(key: string): boolean {
	return ID.test(key);
}

const Id = v.pipe(v.string(), v.regex(ID, 'is not 16 lowercase hexadecimal digits'));

const Revision = v.pipe(
	v.number(),
	v.safeInteger('is not a whole number'),
	v.minValue(0, 'is below 0'),
);

/** One DNS label, and never an id's shape, which a route would read as an id. */
const Label = v.pipe(
	v.string(),
	v.regex(/^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/, 'is not one DNS label'),
	v.check((name) => !isId(name), 'has the shape of an id'),
);

/** A JSON object; `v.record` alone takes a list too, and hands it on as an empty object. */
const Spec = v.custom<Record<string, unknown>>(
	(spec) => typeof spec === 'object' && spec !== null && !Array.isArray(spec),
	'is not an object',
);

/** A node and an account are their ids alone; the database never holds a node's old name. */
const Unnamed = v.strictObject({ id: Id, name: v.null(), revision: Revision, spec: Spec });

/** An app's placements are node ids now, which `?node=` filters on. */
const App = v.strictObject({
	id: Id,
	name: Label,
	revision: Revision,
	spec: v.looseObject({ placements: v.array(Id) }),
});

const Body = v.strictObject({
	revision: Revision,
	nodes: v.array(Unnamed),
	apps: v.array(App),
	accounts: v.array(Unnamed),
});

/** An import as it is written, or why it is not. */
export type Imported = { revision: number; rows: Row[] } | { refusal: string };

/** `value` as JSON with every object's keys sorted, so equal specs are equal text. */
export function canonical(value: unknown): string {
	return JSON.stringify(sorted(value));
}

function sorted(value: unknown): unknown {
	if (Array.isArray(value)) return value.map(sorted);
	if (value === null || typeof value !== 'object') return value;
	const entries = Object.entries(value).toSorted(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
	return Object.fromEntries(entries.map(([key, inner]) => [key, sorted(inner)]));
}

function row(kind: Kind, item: v.InferOutput<typeof Unnamed | typeof App>): Row {
	const { id, name, revision } = item;
	return { kind, id, name, revision, spec: canonical(item.spec) };
}

/** The body checked: its shape, and what holds between its parts. */
export function imported(body: unknown): Imported {
	const parsed = v.safeParse(Body, body);
	if (!parsed.success) {
		const [issue] = parsed.issues;
		const path = issue ? v.getDotPath(issue) : null;
		return { refusal: `${path ?? 'the body'} ${issue?.message ?? 'is not an import'}` };
	}
	const { revision, nodes, apps, accounts } = parsed.output;
	const rows = [
		...nodes.map((node) => row('node', node)),
		...apps.map((app) => row('app', app)),
		...accounts.map((account) => row('account', account)),
	];
	const ids = new Set<string>();
	for (const { id, revision: at } of rows) {
		if (ids.has(id)) return { refusal: `${id} is held twice` };
		ids.add(id);
		if (at > revision) return { refusal: `${id} is at ${at}, past the import's ${revision}` };
	}
	const nodeIds = new Set(nodes.map((node) => node.id));
	const names = new Set<string>();
	for (const { name, spec } of apps) {
		if (names.has(name)) return { refusal: `the app ${name} is held twice` };
		names.add(name);
		const placed = new Set<string>();
		for (const placement of spec.placements) {
			if (!nodeIds.has(placement)) return { refusal: `${name} is placed on ${placement}, no node` };
			if (placed.has(placement)) return { refusal: `${name} is placed on ${placement} twice` };
			placed.add(placement);
		}
	}
	return { revision, rows };
}
