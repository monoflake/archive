/**
 * A ledger over HTTP for the thread tests: every step begun is started, every result kept, every
 * end recorded, and the registry's one route answered, so a thread reaches it as it would the
 * private side.
 */

import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';

export interface HttpLedger {
	readonly base: string;
	readonly written: { method: string; path: string; body: Record<string, unknown> }[];
	close(): Promise<void>;
}

export async function httpLedger(): Promise<HttpLedger> {
	const written: HttpLedger['written'] = [];
	const server = createServer((request, response) => {
		let text = '';
		request.on('data', (chunk: Buffer) => {
			text += chunk.toString();
		});
		request.on('end', () => {
			const path = request.url ?? '';
			written.push({ method: request.method ?? '', path, body: text ? JSON.parse(text) : {} });
			const data = path.endsWith('/attempts')
				? { status: 'started', attempt: 1, deadline: '2026-10-14T12:05:00Z' }
				: /\/steps\/[^/]+$/.test(path)
					? { next: 'continue' }
					: path === '/registry/v1/apps/store'
						? { placements: ['rdu', 'buf'] }
						: {};
			response.setHeader('content-type', 'application/json');
			response.end(JSON.stringify({ status: 'success', data }));
		});
	});
	await new Promise<void>((resolve) => server.listen(0, resolve));
	const { port } = server.address() as AddressInfo;
	return {
		base: `http://localhost:${port}`,
		written,
		close: () => new Promise((resolve) => server.close(() => resolve())),
	};
}
