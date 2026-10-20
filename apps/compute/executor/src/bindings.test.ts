import { describe, expect, it } from 'vitest';
import { addressOf, envOf } from './bindings.ts';

describe('a binding', () => {
	it('sends the path and query of what it is asked to its base, whatever the host', () => {
		expect(addressOf('http://service.invalid/service', '/v1/things/one')).toBe(
			'http://service.invalid/service/v1/things/one',
		);
		expect(
			addressOf(
				'http://s.invalid/service/',
				'https://anything.invalid/v1/things?node=0000000000000007',
			),
		).toBe('http://s.invalid/service/v1/things?node=0000000000000007');
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
		const env = envOf({ SERVICE: 'http://s.invalid/service' }, reach);
		await env.SERVICE?.fetch('/v1/things', { method: 'PATCH', body: '{}' });
		await env.SERVICE?.fetch(new Request('http://x.invalid/v1/things/one', { method: 'DELETE' }));
		expect(asked.map((request) => `${request.method} ${request.url}`)).toEqual([
			'PATCH http://s.invalid/service/v1/things',
			'DELETE http://s.invalid/service/v1/things/one',
		]);
	});
});
