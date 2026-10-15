/**
 * control's routes, apart from the Worker so a test can stand a database in: `/health`, the
 * resources under `/v1` as the old registry answers them, and the import that copies them across.
 * Every route but `/health` takes a bearer. See RFC o7eir.
 */
import { failure, success } from '@canmi/response';
import { envelopeErrors } from '@canmi/response/hono';
import { Hono, type MiddlewareHandler } from 'hono';
import { admitted } from './auth.ts';
import { imported, isId } from './import.ts';
import { answers, type Database, found, type Kind, listed, replaced } from './store.ts';

export interface Env {
	readonly DB: Database;
	readonly CONTROL_READ_TOKEN?: string;
	readonly CONTROL_IMPORT_TOKEN?: string;
}

type App = { Bindings: Env };

const KINDS = new Map<string, Kind>([
	['nodes', 'node'],
	['apps', 'app'],
	['accounts', 'account'],
]);

/** The bearer `token` names, or a refusal: closed while it is not set. */
function bearer(token: 'CONTROL_READ_TOKEN' | 'CONTROL_IMPORT_TOKEN'): MiddlewareHandler<App> {
	return async (c, next) => {
		const expected = c.env[token];
		if (!expected) {
			const message = `This route is closed: control has no ${token} set`;
			return failure(503, 'service_unavailable', { message });
		}
		if (!(await admitted(c.req.header('Authorization') ?? null, expected))) {
			return failure(401, 'invalid_token');
		}
		await next();
	};
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

export const app = new Hono<App>();

app.get('/health', async (c) => {
	try {
		await answers(c.env.DB);
		return success({});
	} catch (error) {
		console.error(error);
		return failure(503, 'store_unavailable');
	}
});
app.all('/health', invalidMethod('GET'));

// Ahead of the reads, so `import` is never taken for a kind.
app.put('/v1/import', bearer('CONTROL_IMPORT_TOKEN'), async (c) => {
	let body: unknown;
	try {
		body = await c.req.json();
	} catch {
		return failure(400, 'invalid_body');
	}
	const checked = imported(body);
	if ('refusal' in checked) return failure(400, 'invalid_body', { message: checked.refusal });
	const at = new Date().toISOString();
	return held(async () => {
		const revision = await replaced(c.env.DB, checked.rows, checked.revision, at);
		if (revision !== checked.revision) {
			const message = `control holds revision ${revision}, past the ${checked.revision} given`;
			return failure(409, 'invalid_revision', { message });
		}
		return success({ revision });
	});
});
app.all('/v1/import', invalidMethod('PUT'));

const reads = new Hono<App>();
reads.use('*', bearer('CONTROL_READ_TOKEN'));

reads.get('/:plural', (c) => {
	const kind = KINDS.get(c.req.param('plural'));
	if (!kind) return failure(404, 'no_such_kind');
	const name = c.req.query('name');
	const node = c.req.query('node');
	if (name !== undefined && kind !== 'app') {
		const message = `${kind === 'node' ? 'A node' : 'An account'} has no name: it is its id`;
		return failure(400, 'invalid_name', { message });
	}
	if (node !== undefined && kind !== 'app') {
		return failure(400, 'forbidden_parameter', { message: 'Only an app is placed on a node' });
	}
	if (node !== undefined && !isId(node)) {
		return failure(400, 'invalid_resource', { message: '`node` is a node id' });
	}
	return held(async () => success(await listed(c.env.DB, kind, { name, node })));
});

/** A key of an id's shape is an id; any other is a name, which only an app has. */
reads.get('/:plural/:key', (c) => {
	const kind = KINDS.get(c.req.param('plural'));
	if (!kind) return failure(404, 'no_such_kind');
	const key = c.req.param('key');
	if (!isId(key) && kind !== 'app') return absent(kind);
	return held(async () => {
		const resource = await found(c.env.DB, kind, isId(key) ? { id: key } : { name: key });
		return resource ? success(resource) : absent(kind);
	});
});
reads.all('/:plural', invalidMethod('GET'));
reads.all('/:plural/:key', invalidMethod('GET'));

app.route('/v1', reads);

envelopeErrors(app);
