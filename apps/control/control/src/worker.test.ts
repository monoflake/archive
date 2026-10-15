import { describe, expect, it, vi } from 'vitest';
import { body, ORIGIN, READ, shown, Sqlite } from './testing.ts';

// workerd's base class, which the runner's stand-in for `cloudflare:workers` does not carry.
vi.mock('cloudflare:workers', () => ({
	WorkerEntrypoint: class {
		constructor(
			protected ctx: unknown,
			protected env: unknown,
		) {}
	},
}));

const { Console, default: worker } = await import('./worker.ts');

/** The Worker's env, D1 stood in for, every token set. */
function bindings() {
	return {
		DB: new Sqlite(),
		CONTROL_READ_TOKEN: READ,
		CONTROL_WRITE_TOKEN: 'write-token',
		CONTROL_IMPORT_TOKEN: 'import-token',
	} as never;
}

function asked(method: string, path: string, sent?: unknown) {
	const json = sent === undefined ? undefined : JSON.stringify(sent);
	return new Request(new URL(path, ORIGIN), { method, body: json });
}

describe('the Console entrypoint', () => {
	it('admits the console to read and write with no bearer', async () => {
		const entry = new Console({} as never, bindings());
		expect((await entry.fetch(asked('GET', '/v1/registry/nodes'))).status).toBe(200);
		const spec = shown('100.64.0.9', 'Tokyo', 'JP');
		const made = await entry.fetch(
			asked('POST', '/v1/registry/nodes', { by: 'console', revision: 0, spec }),
		);
		expect(made.status).toBe(202);
	});

	it('does not admit the console to the import', async () => {
		const entry = new Console({} as never, bindings());
		const answer = await entry.fetch(asked('PUT', '/v1/registry/import', body()));
		expect(answer.status).toBe(401);
	});
});

describe('the default export', () => {
	it('still refuses a caller with no bearer', async () => {
		const answer = await worker.fetch(asked('GET', '/v1/registry/nodes') as never, bindings());
		expect(answer.status).toBe(401);
		expect(await answer.json()).toMatchObject({ code: 'invalid_token' });
	});
});
