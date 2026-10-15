/**
 * What a write says and whether its spec is stored, as the old registry decided both: the body
 * `{by, revision, spec}`, a merge patch laid over the spec held, and each kind's checks, the ones
 * that read the database among them. See RFC o7eir and spec/architecture/control.md.
 */
import { app, account, identities, isObject, node, type Refusal, type Spec } from './specs.ts';
import { clashing, type Database, found, type Kind, type Resource, unplaceable } from './store.ts';

/** What a write's body says: who, the revision it was read at, and the spec wanted. */
export interface Written {
	by: string;
	revision: number;
	spec: unknown;
}

const invalid = (message: string): Refusal => ({ status: 400, code: 'invalid_body', message });

/**
 * The body of a write, or its refusal. `status` and `drift` are a controller's and the platform's
 * to write, never a person's, so a body carrying either is refused whatever it holds, and so is a
 * `name` for a kind that has none.
 */
export function written(kind: Kind, body: unknown): Written | Refusal {
	if (!isObject(body)) return invalid('the body is an object of `by`, `revision` and `spec`');
	const reported = ['status', 'drift'].find((key) => key in body);
	if (reported !== undefined) {
		const message = `\`${reported}\` is not written by a person`;
		return { status: 400, code: 'forbidden_field', message };
	}
	if ('name' in body && kind !== 'app') {
		return { status: 400, code: 'forbidden_field', message: `A ${kind} has no name to write` };
	}
	const { by, revision, spec } = body;
	if (typeof by !== 'string' || by.trim() === '') return invalid('`by` names who');
	if (!Number.isSafeInteger(revision) || (revision as number) < 0) {
		return invalid('`revision` is the one read, 0 for what does not exist yet');
	}
	if (spec === undefined) return invalid('`spec` is required');
	return { by, revision: revision as number, spec };
}

/** `patch` laid over `target` as RFC 7386 lays a merge patch, a key given `null` taken out. */
export function merged(target: unknown, patch: unknown): unknown {
	if (!isObject(patch)) return patch;
	const result = new Map(Object.entries(isObject(target) ? target : {}));
	for (const [key, value] of Object.entries(patch)) {
		if (value === null) result.delete(key);
		else result.set(key, merged(result.get(key), value));
	}
	return Object.fromEntries(result);
}

/**
 * Why `spec` is not stored as `kind`'s, if it is not: its own checks, then what it names held as
 * they need to be -- each node an app is placed on anew held and not retired, so retiring a node
 * never refuses the next release of an app already placed there; a node's account one held; and no
 * other account at an account's provider holding one of its identities. `kept` is the resource
 * written as it is held, none for one being made. Read before the write's batch; an account's
 * identities are held to their key in it as well.
 */
export async function checked(
	db: Database,
	kind: Kind,
	spec: unknown,
	kept: Resource | undefined,
): Promise<Refusal | undefined> {
	const refusal = { node, app, account }[kind](spec);
	if (refusal !== undefined) return refusal;
	const held = spec as Spec;
	if (kind === 'app') {
		const before = isObject(kept?.spec) ? kept.spec.placements : undefined;
		const placed = new Set(Array.isArray(before) ? before : []);
		const added = (held.placements as string[]).filter((id) => !placed.has(id));
		const [absent] = await unplaceable(db, added);
		if (absent === undefined) return undefined;
		const message = `\`placements\` names ${absent}, no node control holds, or one retired`;
		return { status: 400, code: 'invalid_resource', message };
	}
	if (kind === 'node' && typeof held.account === 'string') {
		if (await found(db, 'account', { id: held.account })) return undefined;
		const message = '`account` names no account control holds';
		return { status: 400, code: 'invalid_resource', message };
	}
	if (kind === 'account') return clash(db, held, kept?.id ?? null);
	return undefined;
}

/**
 * An identity another account at this provider holds, named by that account's first eight digits
 * and the identity's kind, never its value.
 */
export async function clash(db: Database, spec: Spec, own: string | null) {
	const other = await clashing(db, identities(spec), own);
	if (other === undefined) return undefined;
	const message = `account ${other.account.slice(0, 8)} at this provider already holds this \`${other.kind}\``;
	return { status: 400, code: 'invalid_resource', message } satisfies Refusal;
}
