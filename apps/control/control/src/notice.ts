/**
 * Change notices: host on every bound node told that something it serves has changed, so it asks
 * then rather than on a timer. `{changed: "registry", revision}` follows every registry write that
 * lands; `{changed: "secrets"}` is the operator's, through the `notice` scope, after a secret is
 * written or removed. Sent to the `infra` receiver alone, never keeper or the deployer; a node that
 * misses one is logged and never fails what sent it.
 */
import { isObject } from './specs.ts';
import { type Bound, nodesOf, noticeAt, type Reached, told } from './nodes.ts';

export type Changed = { changed: 'registry'; revision: number } | { changed: 'secrets' };

/** host's door on each node, which Caddy passes on to host. */
export const HOST = noticeAt('infra');

/** `notice` sent to host on every bound node, at once, each miss logged with the node it was. */
export async function announced(env: Bound, notice: Changed): Promise<Reached[]> {
	const body = JSON.stringify(notice);
	const reached = await Promise.all(
		[...nodesOf(env)].map(([node, binding]) => told(node, binding, HOST, body)),
	);
	for (const { node, status, ok } of reached) {
		if (!ok) console.error(`${notice.changed} notice: ${node} did not take it: ${status}`);
	}
	return reached;
}

/** What the `notice` scope's body asks to be told: `changed` and nothing else, else none. */
export function asked(body: unknown): Changed['changed'] | undefined {
	if (!isObject(body) || Object.keys(body).length !== 1) return undefined;
	const { changed } = body;
	return changed === 'registry' || changed === 'secrets' ? changed : undefined;
}
