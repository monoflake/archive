/** `control` as deployed on Workers: the routes in index.ts, over D1. See RFC o7eir. */
import { app } from './index.ts';

interface Env {
	readonly DB: D1Database;
	readonly CONTROL_READ_TOKEN?: string;
	readonly CONTROL_WRITE_TOKEN?: string;
	readonly CONTROL_IMPORT_TOKEN?: string;
}

export default {
	fetch: (request, env) => app.fetch(request, env),
} satisfies ExportedHandler<Env>;
