import { describe, expect, it } from 'vitest';
import { addressOf, envOf } from './bindings.ts';

describe('a binding', () => {
	it('sends the path and query of what it is asked to its base, whatever the host', () => {
		expect(addressOf('http://registry.invalid/registry', '/v1/apps/store')).toBe(
			'http://registry.invalid/registry/v1/apps/store',
		);
		expect(
			addressOf('http://r.invalid/registry/', 'https://anything.invalid/v1/apps?node=rdu'),
		).toBe('http://r.invalid/registry/v1/apps?node=rdu');
		expect(addressOf('http://r.invalid', new Request('http://x.invalid/v1/kinds'))).toBe(
			'http://r.invalid/v1/kinds',
		);
	});

	it('is a fetch per declared name, carrying the request it was given', async () => {
		const asked: Request[] = [];
		const reach = (async (input: RequestInfo | URL, init?: RequestInit) => {
			asked.push(new Request(input, init));
			return new Response('ok');
		}) as typeof fetch;
		const env = envOf({ REGISTRY: 'http://r.invalid/registry' }, reach);
		await env.REGISTRY?.fetch('/v1/apps', { method: 'PATCH', body: '{}' });
		await env.REGISTRY?.fetch(new Request('http://x.invalid/v1/apps/store', { method: 'DELETE' }));
		expect(asked.map((request) => `${request.method} ${request.url}`)).toEqual([
			'PATCH http://r.invalid/registry/v1/apps',
			'DELETE http://r.invalid/registry/v1/apps/store',
		]);
	});
});
