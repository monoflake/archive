import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { Failure } from './failure.ts';
import { type SecretWorld, runSecret } from './secret.ts';

const TOKEN = 'token-for-the-fake';
const VALUE = 'typed-s3cret';

interface Sent {
	method: string;
	path: string;
	auth: string | undefined;
	body: Record<string, unknown> | undefined;
}

interface Held {
	scope: string;
	app: string;
	name: string;
	class: 'config' | 'secret';
	revision: number;
	value: string;
}

/** The secret service as its routes answer, and every request it was sent. */
class Service {
	revision = 4;
	requests: Sent[] = [];
	held: Held[] = [
		{ scope: 'platform', app: 'geo', name: 'LEVEL', class: 'config', revision: 3, value: 'debug' },
		{
			scope: 'platform',
			app: 'geo',
			name: 'TOKEN',
			class: 'secret',
			revision: 4,
			value: 'held-s3cret',
		},
		{
			scope: 'canmi',
			app: 'shot',
			name: 'KEY',
			class: 'secret',
			revision: 2,
			value: 'other-s3cret',
		},
	];

	item(held: Held) {
		const { value, ...named } = held;
		return {
			...named,
			changed: '2026-10-14T00:00:00Z',
			...(held.class === 'config' ? { value } : {}),
		};
	}

	answer(sent: Sent): [number, unknown] {
		this.requests.push(sent);
		if (sent.auth !== `Bearer ${TOKEN}`) {
			return [401, { status: 'error', code: 'invalid_token', message: 'No token' }];
		}
		const [pathname = '', query = ''] = sent.path.split('?');
		if (pathname === '/v1/secrets') {
			const items = this.held.map((held) => this.item(held));
			return [200, { status: 'success', data: { items, revision: this.revision } }];
		}
		const [, , , app, name] = pathname.split('/');
		const scope = new URLSearchParams(query).get('scope') ?? 'platform';
		const at = this.held.findIndex(
			(one) => one.scope === scope && one.app === app && one.name === name,
		);
		const found = this.held[at];
		const missing: [number, unknown] = [
			404,
			{ status: 'error', code: 'no_such_resource', message: 'None' },
		];
		if (sent.method === 'GET')
			return found ? [200, { status: 'success', data: this.item(found) }] : missing;
		const body = sent.body!;
		if (body.revision !== (found?.revision ?? 0)) {
			return [409, { status: 'error', code: 'invalid_revision', message: 'Changed since' }];
		}
		if (sent.method === 'DELETE') {
			if (!found) return missing;
			this.held.splice(at, 1);
			this.revision += 1;
			return [200, { status: 'success', data: { revision: this.revision } }];
		}
		if (found && found.value === body.value && found.class === body.class) {
			return [200, { status: 'success', data: {} }];
		}
		this.revision += 1;
		const written = {
			scope,
			app: app!,
			name: name!,
			class: body.class as Held['class'],
			revision: this.revision,
			value: body.value as string,
		};
		if (found) this.held[at] = written;
		else this.held.push(written);
		return [200, { status: 'success', data: { revision: this.revision } }];
	}
}

let state: Service;
let server: Server;
let printed: string[];
let asked: string[];
let token: string | undefined;
let typed: string;

beforeEach(async () => {
	state = new Service();
	printed = [];
	asked = [];
	token = TOKEN;
	typed = VALUE;
	server = createServer((request, response) => {
		let text = '';
		request.on('data', (chunk: Buffer) => (text += chunk.toString()));
		request.on('end', () => {
			const [code, answer] = state.answer({
				method: request.method ?? '',
				path: request.url ?? '',
				auth: request.headers.authorization,
				body: text ? JSON.parse(text) : undefined,
			});
			response.writeHead(code, { 'Content-Type': 'application/json' });
			response.end(JSON.stringify(answer));
		});
	});
	await new Promise<void>((done) => server.listen(0, '127.0.0.1', done));
});

afterEach(async () => {
	await new Promise((done) => server.close(done));
});

function world(): SecretWorld {
	return {
		placements: () => ['tyo', 'sin', 'rdu'],
		address: () => '127.0.0.1',
		port: (server.address() as AddressInfo).port,
		token: () => token,
		typedIn: async (prompt) => {
			asked.push(prompt);
			return typed;
		},
		print: (text) => printed.push(text),
	};
}

async function task(...args: string[]): Promise<{ out: string; failed: string | undefined }> {
	let failed: string | undefined;
	try {
		await runSecret(world(), args);
	} catch (error) {
		if (!(error instanceof Failure)) throw error;
		failed = error.message;
	}
	return { out: printed.join('\n'), failed };
}

const writes = () => state.requests.filter((sent) => sent.method !== 'GET');

describe('platform secret', () => {
	it('lists every value, a config with its value and a secret by its name alone', async () => {
		const { out, failed } = await task('list');
		expect(failed).toBeUndefined();
		expect(out).toContain('debug');
		expect(out).not.toContain('s3cret');
		expect(out.split('\n')).toHaveLength(4);
		const geo = await task('list', 'geo', '--json');
		const items = JSON.parse(geo.out.slice(geo.out.indexOf('['))) as { name: string }[];
		expect(items.map((item) => item.name)).toEqual(['LEVEL', 'TOKEN']);
	});

	it('sets a new value as a secret at revision 0, the value typed in and never printed', async () => {
		const { out, failed } = await task('set', 'geo', 'API_KEY');
		expect(failed).toBeUndefined();
		const [sent] = writes();
		expect([sent!.method, sent!.path, sent!.auth]).toEqual([
			'PUT',
			'/v1/secrets/geo/API_KEY',
			`Bearer ${TOKEN}`,
		]);
		expect(sent!.body).toEqual({ by: 'operator', revision: 0, value: VALUE, class: 'secret' });
		expect(asked).toEqual(['platform/geo/API_KEY (secret): ']);
		expect(out).toBe('platform: platform/geo/API_KEY: written as secret, revision 5');
		expect(out).not.toContain(VALUE);
	});

	it('sets a config over a value at the revision read, in the scope named', async () => {
		await task('set', 'geo', 'LEVEL', '--config');
		expect(writes()[0]!.body).toEqual({
			by: 'operator',
			revision: 3,
			value: VALUE,
			class: 'config',
		});
		await task('set', 'shot', 'KEY', '--scope', 'canmi');
		const [, scoped] = writes();
		expect([scoped!.path, scoped!.body!.revision]).toEqual(['/v1/secrets/shot/KEY?scope=canmi', 2]);
	});

	it('says unchanged for a value and class already held', async () => {
		typed = 'debug';
		const { out } = await task('set', 'geo', 'LEVEL', '--config');
		expect(out).toBe('platform: platform/geo/LEVEL: unchanged');
	});

	it('deletes at the revision read, and refuses a value not held', async () => {
		const { out } = await task('delete', 'geo', 'TOKEN');
		const [sent] = writes();
		expect([sent!.method, sent!.path]).toEqual(['DELETE', '/v1/secrets/geo/TOKEN']);
		expect(sent!.body).toEqual({ by: 'operator', revision: 4 });
		expect(out).toBe('platform: platform/geo/TOKEN: removed, revision 5');
		const { failed } = await task('delete', 'geo', 'TOKEN');
		expect(failed).toBe('platform/geo/TOKEN: no such value');
		expect(writes()).toHaveLength(1);
	});

	it('asks for nothing and writes nothing on a dry run', async () => {
		const set = await task('set', 'geo', 'LEVEL', '--config', '--dry-run');
		expect(set.out).toContain('would PUT /v1/secrets/geo/LEVEL as config at revision 3');
		const removed = await task('delete', 'geo', 'TOKEN', '--dry-run');
		expect(removed.out).toContain('would DELETE /v1/secrets/geo/TOKEN at revision 4');
		expect(asked).toEqual([]);
		expect(writes()).toEqual([]);
	});

	it('refuses a name of the wrong shape, an empty value, or no token, before writing', async () => {
		expect((await task('set', 'Geo', 'KEY')).failed).toContain('no app');
		expect((await task('set', 'geo', 'lower')).failed).toContain('no variable');
		expect((await task('set', 'geo')).failed).toContain('mise run platform secret');
		expect((await task('delete', 'geo', 'TOKEN', '--config')).failed).toContain('--config');
		expect((await task('rotate', 'geo', 'TOKEN')).failed).toContain('mise run platform secret');
		typed = '';
		expect((await task('set', 'geo', 'KEY')).failed).toContain('no value given');
		token = undefined;
		expect((await task('list')).failed).toContain('SECRETS_TOKEN');
		expect(writes()).toEqual([]);
	});

	it('fails a refusal with its code', async () => {
		token = 'wrong';
		const { failed } = await task('list');
		expect(failed).toBe('GET /v1/secrets: refused invalid_token: No token');
	});
});
