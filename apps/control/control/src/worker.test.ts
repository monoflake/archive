import { describe, expect, it, vi } from 'vitest';
import { body, context, ORIGIN, READ, shown, Sqlite } from './testing.ts';

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
		const entry = new Console(context().ctx as never, bindings());
		expect((await entry.fetch(asked('GET', '/v1/registry/nodes'))).status).toBe(200);
		const spec = shown('100.64.0.9', 'Tokyo', 'JP');
		const made = await entry.fetch(
			asked('POST', '/v1/registry/nodes', { by: 'console', revision: 0, spec }),
		);
		expect(made.status).toBe(202);
	});

	it("hands a write's change notice to the context it was given", async () => {
		const { ctx, waited } = context();
		const entry = new Console(ctx as never, bindings());
		const spec = shown('100.64.0.9', 'Tokyo', 'JP');
		await entry.fetch(asked('POST', '/v1/registry/nodes', { by: 'console', revision: 0, spec }));
		expect(waited).toHaveLength(1);
	});

	it('does not admit the console to the import', async () => {
		const entry = new Console({} as never, bindings());
		const answer = await entry.fetch(asked('PUT', '/v1/registry/import', body()));
		expect(answer.status).toBe(401);
	});
});

describe('the default export', () => {
	it('still refuses a caller with no bearer', async () => {
		const answer = await worker.fetch(
			asked('GET', '/v1/registry/nodes') as never,
			bindings(),
			context().ctx as never,
		);
		expect(answer.status).toBe(401);
		expect(await answer.json()).toMatchObject({ code: 'invalid_token' });
	});

	it("hands a write's change notice to the context it was given", async () => {
		const { ctx, waited } = context();
		const spec = shown('100.64.0.9', 'Tokyo', 'JP');
		const request = new Request(new URL('/v1/registry/nodes', ORIGIN), {
			method: 'POST',
			headers: { Authorization: 'Bearer write-token' },
			body: JSON.stringify({ by: 'test', revision: 0, spec }),
		});
		expect((await worker.fetch(request as never, bindings(), ctx as never)).status).toBe(202);
		expect(waited).toHaveLength(1);
	});
});
