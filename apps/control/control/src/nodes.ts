/**
 * Every node control is bound to, over Workers VPC, and how one is told something: a `POST` of a
 * JSON notice to a receiver on that node's Caddy, through the node's tunnel. The `hook` scope and
 * the change notices both send through it.
 */
import { URLS } from '@monoflake/sdk';

/** The part of a VPC binding used here: a node's Caddy, through that node's tunnel. */
export interface Node {
	fetch(url: string, init: RequestInit): Promise<Response>;
}

/** A node's VPC binding, as wrangler.jsonc names each: `NODE_` and the node's id in capitals. */
export type Binding = `NODE_${string}`;

export interface Bound {
	readonly [binding: Binding]: Node | undefined;
}

const PREFIX = 'NODE_';

export function bindingOf(id: string): Binding {
	return `${PREFIX}${id.toUpperCase()}`;
}

function isNode(value: unknown): value is Node {
	return typeof (value as Node | undefined)?.fetch === 'function';
}

/** Every node bound, by its id. */
export function nodesOf(env: Bound): Map<string, Node> {
	const nodes = new Map<string, Node>();
	for (const [name, value] of Object.entries(env)) {
		if (name.startsWith(PREFIX) && isNode(value)) {
			nodes.set(name.slice(PREFIX.length).toLowerCase(), value);
		}
	}
	return nodes;
}

/** The public suffix Caddy routes every receiver under; VPC sends it as the `Host`. */
const SUFFIX = new URL(URLS.internal.app).hostname;

/** The notice receiver of `label` on a node. */
export function noticeAt(label: string): string {
	return `http://${label}.${SUFFIX}/notice`;
}

export interface Reached {
	node: string;
	receiver: string;
	status: number | string;
	ok: boolean;
}

/** `notice` posted to `receiver` on `node`; a binding that throws is a miss, never a throw. */
export async function told(
	node: string,
	binding: Node,
	receiver: string,
	notice: string,
): Promise<Reached> {
	try {
		const answer = await binding.fetch(receiver, {
			method: 'POST',
			headers: { 'content-type': 'application/json' },
			body: notice,
		});
		return { node, receiver, status: answer.status, ok: answer.ok };
	} catch (error) {
		return { node, receiver, status: String(error), ok: false };
	}
}
