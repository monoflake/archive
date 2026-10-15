/**
 * `control` as deployed on Workers: the routes in index.ts, over D1, for a bearer by default and
 * for the console through `Console`. See RFC o7eir.
 */
import { WorkerEntrypoint } from 'cloudflare:workers';
import type { Binding } from './hook.ts';
import { app, CONSOLE } from './index.ts';

interface Env {
	readonly DB: D1Database;
	readonly CONTROL_READ_TOKEN?: string;
	readonly CONTROL_WRITE_TOKEN?: string;
	readonly CONTROL_IMPORT_TOKEN?: string;
	readonly WEBHOOK_SECRET?: string;
	readonly [binding: Binding]: Fetcher | undefined;
}

// Only a same-account Worker can bind a named entrypoint, so the binding proves the caller.
export class Console extends WorkerEntrypoint<Env> {
	override fetch(request: Request): Response | Promise<Response> {
		return app.fetch(request, { ...this.env, [CONSOLE]: true });
	}
}

export default {
	fetch: (request, env) => app.fetch(request, env),
} satisfies ExportedHandler<Env>;
