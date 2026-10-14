/**
 * The executor's two routes: `/turns`, the doorbell the ledger rings, which only polls -- whatever
 * it carries is ignored, since the turn comes from the poll -- and `/health` for host. See
 * spec/architecture/executor.md, "Polls, and a push that is a doorbell".
 */

import { failure, success } from '@canmi/response';

export interface Routes {
	readonly ring: () => void;
	readonly health: () => Record<string, unknown>;
}

export async function handle(request: Request, routes: Routes): Promise<Response> {
	const { pathname } = new URL(request.url);
	if (pathname === '/health' && request.method === 'GET') return success(routes.health());
	if (pathname === '/turns' && request.method === 'POST') {
		await request.body?.cancel();
		routes.ring();
		return success({}, { status: 202 });
	}
	return failure(404, 'no_such_route');
}
