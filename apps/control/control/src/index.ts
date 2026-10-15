/**
 * control's routes, apart from the Worker so a test can stand a database in: `/v1/health`; the
 * `registry` scope at `/v1/registry`: the resources read and written as the old registry answers
 * them, the catalog of their kinds, and the import that copies them across; and the `hook` scope,
 * GitHub's deploy runs passed on to the nodes. Every route but `/v1/health` and the hook takes a
 * bearer, or the console by its binding. See RFC o7eir and spec/architecture/control.md.
 */
import { failure, success } from '@canmi/response';
import { envelopeErrors } from '@canmi/response/hono';
import { poweredBy } from '@canmi/web/disclose/hono';
import { type Context, Hono, type MiddlewareHandler } from 'hono';
import { admitted } from './auth.ts';
import { delivered, type Env as Hooked } from './hook.ts';
import { canonical, imported } from './import.ts';
import { CATALOG } from './kinds.ts';
import { identities, isId, isLabel, isObject, type Refusal, type Spec } from './specs.ts';
import {
	answers,
	type Change,
	clashed,
	committed,
	type Database,
	found,
	type Kind,
	listed,
	replaced,
	type Resource,
} from './store.ts';
import { checked, clash, merged, type Written, written } from './write.ts';

/**
 * Marks a request the console made, through worker.ts's `Console`: a symbol, so no binding, secret
 * or variable can be named it.
 */
export const CONSOLE: unique symbol = Symbol('console');

export interface Env extends Hooked {
	readonly DB: Database;
	readonly [CONSOLE]?: true;
	readonly CONTROL_READ_TOKEN?: string;
	readonly CONTROL_WRITE_TOKEN?: string;
	readonly CONTROL_IMPORT_TOKEN?: string;
}

type App = { Bindings: Env };

const KINDS = new Map<string, Kind>([
	['nodes', 'node'],
	['apps', 'app'],
	['accounts', 'account'],
]);

/** What a kind's collection and each of its members take, as `Allow` says it. */
const ALLOWED: Record<Kind, { list: string; one: string }> = {
	node: { list: 'GET, POST', one: 'GET, PATCH' },
	app: { list: 'GET', one: 'GET, PUT, PATCH' },
	account: { list: 'GET, POST', one: 'GET, PATCH' },
};

const A: Record<Kind, string> = { node: 'A node', app: 'An app', account: 'An account' };

type Token = 'CONTROL_READ_TOKEN' | 'CONTROL_WRITE_TOKEN' | 'CONTROL_IMPORT_TOKEN';

/**
 * A bearer one of `tokens` names, or a refusal: closed while none of them is set. Each set token is
 * compared, whichever matches, so the timing says nothing of which one a bearer is.
 */
function bearer(...tokens: Token[]): MiddlewareHandler<App> {
	return async (c, next) => {
		const expected = tokens.map((token) => c.env[token]).filter((token) => !!token) as string[];
		if (expected.length === 0) {
			const message = `This route is closed: control has no ${tokens.join(' or ')} set`;
			return failure(503, 'service_unavailable', { message });
		}
		const given = c.req.header('Authorization') ?? null;
		const matched = await Promise.all(expected.map((token) => admitted(given, token)));
		if (!matched.includes(true)) return failure(401, 'invalid_token');
		await next();
	};
}

/** The console, admitted by its binding, or else what `guard` admits. */
function orConsole(guard: MiddlewareHandler<App>): MiddlewareHandler<App> {
	return (c, next) => (c.env[CONSOLE] ? next() : guard(c, next));
}

const invalidMethod = (allowed: string) => () =>
	failure(405, 'invalid_method', { headers: { Allow: allowed } });

/** A database call, its failure answered as the old registry answers it. */
async function held(answer: () => Promise<Response>): Promise<Response> {
	try {
		return await answer();
	} catch (error) {
		console.error(error);
		return failure(500, 'store_unavailable');
	}
}

/** What a kind answers for a resource it does not hold, as the old registry answers it. */
function absent(kind: Kind): Response {
	return failure(404, kind === 'node' ? 'no_such_node' : 'no_such_resource');
}

function refused({ status, code, message }: Refusal): Response {
	return failure(status, code, { message });
}

/** What a name answers where a kind has none: refused, saying the id is what names one. */
function unnamed(kind: Kind): Response {
	const message = `${A[kind]} has no name: it is its id, made by POST /v1/registry/${kind}s`;
	return failure(400, 'invalid_name', { message });
}

/** The body sent, or nothing where it is not JSON, which `written` refuses. */
async function sent(c: Context<App>): Promise<unknown> {
	return c.req.json().catch(() => undefined);
}

export const app = new Hono<App>();

// First, so every answer says what made it. See lib's spec/web/disclose.md.
app.use(poweredBy());

// The `health` scope: whether control and its database answer, the one route that takes no bearer.
app.get('/v1/health', async (c) => {
	try {
		await answers(c.env.DB);
		return success({});
	} catch (error) {
		console.error(error);
		return failure(503, 'store_unavailable');
	}
});
app.all('/v1/health', invalidMethod('GET'));

// The write token reads too: a writer reads what it is about to hold its write to.
const read = orConsole(bearer('CONTROL_READ_TOKEN', 'CONTROL_WRITE_TOKEN'));
const write = orConsole(bearer('CONTROL_WRITE_TOKEN'));
/** The `registry` scope: the nodes, the apps and the accounts, and their catalog. */
const registry = new Hono<App>();

// Ahead of the kinds, so `import` is never taken for one.
registry.put('/import', bearer('CONTROL_IMPORT_TOKEN'), async (c) => {
	let body: unknown;
	try {
		body = await c.req.json();
	} catch {
		return failure(400, 'invalid_body');
	}
	const taken = imported(body);
	if ('refusal' in taken) return failure(400, 'invalid_body', { message: taken.refusal });
	const at = new Date().toISOString();
	return held(async () => {
		const { rows, identities: owned, revision: given } = taken;
		const revision = await replaced(c.env.DB, rows, owned, given, at);
		if (revision !== given) {
			const message = `control holds revision ${revision}, past the ${given} given`;
			return failure(409, 'invalid_revision', { message });
		}
		return success({ revision });
	});
});
registry.all('/import', invalidMethod('PUT'));

registry.get('/kinds', read, () => success(CATALOG));
registry.all('/kinds', invalidMethod('GET'));

registry.get('/:plural', read, (c) => {
	const kind = KINDS.get(c.req.param('plural'));
	if (!kind) return failure(404, 'no_such_kind');
	const name = c.req.query('name');
	const node = c.req.query('node');
	if (name !== undefined && kind !== 'app') {
		return failure(400, 'invalid_name', { message: `${A[kind]} has no name: it is its id` });
	}
	if (node !== undefined && kind !== 'app') {
		return failure(400, 'forbidden_parameter', { message: 'Only an app is placed on a node' });
	}
	if (node !== undefined && !isId(node)) {
		return failure(400, 'invalid_resource', { message: '`node` is a node id' });
	}
	return held(async () => success(await listed(c.env.DB, kind, { name, node })));
});

/** Makes a node or an account at revision 0, its id drawn by control and answered. */
registry.post('/:plural', write, async (c) => {
	const kind = KINDS.get(c.req.param('plural'));
	if (!kind) return failure(404, 'no_such_kind');
	if (kind === 'app') {
		const message = 'An app is made by PUT /v1/registry/apps/<name>';
		return failure(405, 'invalid_method', { message, headers: { Allow: ALLOWED.app.list } });
	}
	const asked = written(kind, await sent(c));
	if ('code' in asked) return refused(asked);
	return held(async () => {
		const refusal = await checked(c.env.DB, kind, asked.spec, undefined);
		if (refusal) return refused(refusal);
		if (asked.revision !== 0) return failure(409, 'invalid_revision');
		return landed(c.env.DB, change(kind, asked, undefined), true);
	});
});

registry.all('/:plural', (c) => {
	const kind = KINDS.get(c.req.param('plural'));
	if (!kind) return failure(404, 'no_such_kind');
	return invalidMethod(ALLOWED[kind].list)();
});

/** A key of an id's shape is an id; any other is a name, which only an app has. */
registry.get('/:plural/:key', read, (c) => {
	const kind = KINDS.get(c.req.param('plural'));
	if (!kind) return failure(404, 'no_such_kind');
	const key = c.req.param('key');
	if (!isId(key) && kind !== 'app') return absent(kind);
	return held(async () => {
		const resource = await found(c.env.DB, kind, isId(key) ? { id: key } : { name: key });
		return resource ? success(resource) : absent(kind);
	});
});

/** Sets an app's whole spec, held to the revision read; at revision 0, by a name, makes it. */
registry.put('/:plural/:key', write, async (c) => {
	const kind = KINDS.get(c.req.param('plural'));
	if (!kind) return failure(404, 'no_such_kind');
	if (kind !== 'app') {
		const message = `${A[kind]} is changed by PATCH /v1/registry/${kind}s/<id>`;
		return failure(405, 'invalid_method', { message, headers: { Allow: ALLOWED[kind].one } });
	}
	const key = c.req.param('key');
	if (!isId(key) && !isLabel(key)) {
		const message = 'A name is one DNS label, and never 16 hexadecimal digits, which is an id';
		return failure(400, 'invalid_name', { message });
	}
	const asked = written(kind, await sent(c));
	if ('code' in asked) return refused(asked);
	return held(async () => {
		const kept = await found(c.env.DB, kind, isId(key) ? { id: key } : { name: key });
		if (!kept && isId(key)) return absent(kind);
		const refusal = await checked(c.env.DB, kind, asked.spec, kept);
		if (refusal) return refused(refusal);
		if (asked.revision !== (kept?.revision ?? 0)) return failure(409, 'invalid_revision');
		if (kept && canonical(asked.spec) === canonical(kept.spec)) return success({});
		return landed(c.env.DB, change(kind, asked, kept ?? key), false);
	});
});

/**
 * Lays `spec` over the one kept as a merge patch, held to the revision read as `PUT` is: an app's
 * release written by a machine and its placements by a person, or a node's `retired` by an
 * operator and the rest by the import, never erase each other.
 */
registry.patch('/:plural/:key', write, async (c) => {
	const kind = KINDS.get(c.req.param('plural'));
	if (!kind) return failure(404, 'no_such_kind');
	const key = c.req.param('key');
	if (!isId(key) && kind !== 'app') return unnamed(kind);
	if (!isId(key) && !isLabel(key)) {
		const message = 'A name is one DNS label, and never 16 hexadecimal digits, which is an id';
		return failure(400, 'invalid_name', { message });
	}
	const asked = written(kind, await sent(c));
	if ('code' in asked) return refused(asked);
	if (!isObject(asked.spec)) {
		return failure(400, 'invalid_body', { message: '`spec` is an object of the fields changed' });
	}
	const patch = asked.spec;
	return held(async () => {
		const kept = await found(c.env.DB, kind, isId(key) ? { id: key } : { name: key });
		if (!kept) return absent(kind);
		if (kept.revision !== asked.revision) return failure(409, 'invalid_revision');
		const spec = merged(kept.spec, patch);
		const refusal = await checked(c.env.DB, kind, spec, kept);
		if (refusal) return refused(refusal);
		if (canonical(spec) === canonical(kept.spec)) return success({});
		return landed(c.env.DB, change(kind, { ...asked, spec }, kept), false);
	});
});

registry.all('/:plural/:key', (c) => {
	const kind = KINDS.get(c.req.param('plural'));
	if (!kind) return failure(404, 'no_such_kind');
	return invalidMethod(ALLOWED[kind].one)();
});

app.route('/v1/registry', registry);

// The `hook` scope proves its caller by GitHub's signature, not by a bearer. RFC o7eir.
app.post('/v1/hook/github', (c) => delivered(c.req.raw, c.env));
app.all('/v1/hook/github', invalidMethod('POST'));

/** Every scope under `/v1`: a path one does not take is no route, and any other scope none. */
const SCOPES = new Set(['health', 'registry', 'hook']);

const unscoped = (c: Context<App>) =>
	SCOPES.has(c.req.param('scope') ?? '')
		? failure(404, 'no_such_route')
		: failure(404, 'no_such_scope');
app.all('/v1/:scope', unscoped);
app.all('/v1/:scope/*', unscoped);

/**
 * The write `asked` makes of `target`: the resource kept, an app's name no app holds yet, or, for
 * none, a resource of a kind with no name, its id drawn as it is written.
 */
function change(
	kind: Kind,
	asked: Written,
	target: Resource | string | undefined,
): Omit<Change, 'id'> & { id?: string } {
	const spec = asked.spec as Spec;
	const kept = typeof target === 'object' ? target : undefined;
	return {
		kind,
		...(kept ? { id: kept.id } : {}),
		name: kept ? kept.name : typeof target === 'string' ? target : null,
		made: kept === undefined,
		revision: kept?.revision ?? 0,
		spec: canonical(spec),
		by: asked.by,
		at: new Date().toISOString(),
		...(kind === 'account' ? { identities: identities(spec) } : {}),
	};
}

/**
 * A write's answer: `202` with the revision it made and its task, and the id of what a `POST`
 * made; and a conflict when the resource changed after the revision given was read. An identity
 * another account came to hold between the check and the batch is refused as the check refuses it.
 */
async function landed(
	db: Database,
	asked: Omit<Change, 'id'> & { id?: string },
	post: boolean,
): Promise<Response> {
	try {
		const wrote = await committed(db, asked);
		if (!wrote) return failure(409, 'invalid_revision');
		const { id, revision, task } = wrote;
		return success(post ? { id, revision, task } : { revision, task }, { status: 202 });
	} catch (error) {
		if (!clashed(error)) throw error;
		const refusal = await clash(db, JSON.parse(asked.spec) as Spec, asked.id ?? null);
		if (refusal) return refused(refusal);
		throw error;
	}
}

envelopeErrors(app);
