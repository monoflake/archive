/**
 * The `hook` scope: a GitHub run worth deploying, passed over Workers VPC to host and keeper on
 * every node, and to the deployer where control places it. See spec/architecture/services.md,
 * "Every node is the same node", and spec/architecture/deployer.md, "One deployer, on one node".
 */
import { failure, success } from '@canmi/response';
import { URLS } from '@monoflake/sdk';
import { runToDeploy, signed } from './github.ts';
import { type Database, found } from './store.ts';

/** The part of a VPC binding used here: a node's Caddy, through that node's tunnel. */
export interface Node {
	fetch(url: string, init: RequestInit): Promise<Response>;
}

/** A node's VPC binding, as wrangler.jsonc names each: `NODE_` and the node's id in capitals. */
export type Binding = `NODE_${string}`;

export interface Env {
	readonly DB: Database;
	/** The secret GitHub signs each delivery with, set as a Worker secret. */
	readonly WEBHOOK_SECRET?: string;
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
function nodesOf(env: Env): Map<string, Node> {
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

function noticeAt(label: string): string {
	return `http://${label}.${SUFFIX}/notice`;
}

/** Where the notice goes on every node, by label: host's door, which Caddy passes on to host, and
 * keeper. See infra's spec/architecture/host.md, "One name inside, and a domain label outside". */
export const RECEIVERS = ['infra', 'keeper'].map(noticeAt);

/** The deployer's, asked only on the nodes control places it on; elsewhere it is not. */
export const DEPLOYER = noticeAt('deployer');

/** The ids of the nodes control places the app `deployer` on; none while it holds no such app. */
async function deployerNodes(db: Database): Promise<string[]> {
	const deployer = await found(db, 'app', { name: 'deployer' });
	const placements = (deployer?.spec as { placements?: unknown } | undefined)?.placements;
	return Array.isArray(placements)
		? placements.filter((id): id is string => typeof id === 'string')
		: [];
}

interface Reached {
	node: string;
	receiver: string;
	status: number | string;
	ok: boolean;
}

async function told(node: string, binding: Node, receiver: string, notice: string) {
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

export async function delivered(request: Request, env: Env): Promise<Response> {
	const body = await request.text();
	if (!(await signed(body, request.headers.get('x-hub-signature-256'), env.WEBHOOK_SECRET ?? ''))) {
		return failure(401, 'invalid_signature');
	}
	// `ping` arrives when the webhook is set up; every other event is simply not one to act on.
	if (request.headers.get('x-github-event') !== 'workflow_run') {
		return new Response(null, { status: 204 });
	}
	const deploy = runToDeploy(JSON.parse(body));
	if (deploy === undefined) return new Response(null, { status: 204 });
	const { run, repository } = deploy;

	let deployers: string[];
	try {
		deployers = await deployerNodes(env.DB);
	} catch (error) {
		console.error(error);
		return failure(500, 'store_unavailable');
	}
	const notice = JSON.stringify({ run, repository });
	const nodes = nodesOf(env);
	const reached: Reached[] = await Promise.all([
		...[...nodes].flatMap(([node, binding]) =>
			RECEIVERS.map((receiver) => told(node, binding, receiver, notice)),
		),
		...deployers.map(async (node) => {
			const binding = nodes.get(node);
			if (binding) return told(node, binding, DEPLOYER, notice);
			return { node, receiver: DEPLOYER, status: 'not bound', ok: false };
		}),
	]);
	const missed = reached.filter(({ ok }) => !ok);
	for (const { node, receiver, status } of missed) {
		console.error(`run ${run}: ${node} did not take it at ${receiver}: ${status}`);
	}
	// A receiver that did not take it fails the delivery, so GitHub shows it and it can be
	// redelivered.
	if (missed.length === 0 && reached.length > 0) {
		return success({ run, repository, reached }, { status: 202 });
	}
	const each = missed.map(({ node, receiver, status }) => `${node} ${receiver} ${status}`);
	return failure(502, 'upstream_unavailable', {
		message: `Run ${run} was not taken by every node: ${each.join(', ') || 'no node is bound'}`,
	});
}
