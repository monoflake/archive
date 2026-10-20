import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { URLS } from '@monoflake/sdk';
import { describe, expect, it } from 'vitest';
import { Control, type Release, Releases, newer } from './releases.ts';
import { Store } from './store.ts';

const COMMIT = 'c9a97660'.padEnd(40, '0');
const RELEASE: Release = { repository: 'monoflake/platform', run: 9, commit: COMMIT };
const CONTROL = URLS.control;
const TYO = '0000000000000001';

/** An app as `GET /v1/registry/apps/{name}` answers it, at `revision`, released by `release`. */
function app(revision: number, release?: Partial<Release>) {
	const spec = release ? { placements: [TYO], release } : { placements: [TYO] };
	return Response.json({ status: 'success', data: { kind: 'app', name: 'geo', revision, spec } });
}

function refused(status: number, code: string) {
	return Response.json({ status: 'error', code, message: code }, { status });
}

/**
 * Control over `answers`, taken in order by each request, recording what each asked; an answer
 * that is an Error is thrown, as an address that cannot be reached throws.
 */
function controlOf(answers: (Response | Error)[], base = `${CONTROL}/`) {
	const asked: { url: string; method: string; body?: unknown }[] = [];
	const fetch = async (url: string, init?: RequestInit) => {
		const { origin, pathname } = new URL(url);
		const body = typeof init?.body === 'string' ? JSON.parse(init.body) : undefined;
		asked.push({
			url: `${origin}${pathname}`,
			method: `${init?.method ?? 'GET'} ${pathname}`,
			body,
		});
		expect(new Headers(init?.headers).get('authorization')).toBe('Bearer write');
		const answer = answers.shift();
		if (!answer || answer instanceof Error) throw answer ?? new Error('nothing more to answer');
		return answer;
	};
	return { asked, control: new Control(base, 'write', fetch) };
}

describe('newer', () => {
	it('moves a release forward only, within one repository', () => {
		expect(newer(RELEASE, undefined)).toBe(true);
		expect(newer(RELEASE, { ...RELEASE, run: 8 })).toBe(true);
		expect(newer(RELEASE, { ...RELEASE, run: 9 })).toBe(false);
		expect(newer(RELEASE, { ...RELEASE, run: 10 })).toBe(false);
		expect(newer(RELEASE, { ...RELEASE, repository: 'monoflake/infra', run: 10 })).toBe(true);
	});
});

describe('control', () => {
	it('writes a newer release held to the revision read, keeping the rest to control', async () => {
		const { asked, control } = controlOf([
			app(30, { ...RELEASE, run: 8 }),
			Response.json({ status: 'success', data: { revision: 31 } }),
		]);
		expect(await control.release('geo', RELEASE)).toBe('written');
		expect(asked).toEqual([
			{
				url: `${CONTROL}/v1/registry/apps/geo`,
				method: 'GET /v1/registry/apps/geo',
				body: undefined,
			},
			{
				url: `${CONTROL}/v1/registry/apps/geo`,
				method: 'PATCH /v1/registry/apps/geo',
				body: { by: 'deployer', revision: 30, spec: { release: RELEASE } },
			},
		]);
	});

	it('writes nothing over the same run or a newer one', async () => {
		for (const run of [9, 10]) {
			const { asked, control } = controlOf([app(30, { ...RELEASE, run })]);
			// oxlint-disable-next-line no-await-in-loop -- one case at a time
			expect(await control.release('geo', RELEASE)).toBe('not newer');
			expect(asked).toHaveLength(1);
		}
	});

	it('reads again and writes once more when another change got in first', async () => {
		const { asked, control } = controlOf([
			app(30),
			refused(409, 'invalid_revision'),
			app(32, { ...RELEASE, run: 8 }),
			Response.json({ status: 'success', data: { revision: 33 } }),
		]);
		expect(await control.release('geo', RELEASE)).toBe('written');
		expect(asked.map(({ method }) => method)).toEqual([
			'GET /v1/registry/apps/geo',
			'PATCH /v1/registry/apps/geo',
			'GET /v1/registry/apps/geo',
			'PATCH /v1/registry/apps/geo',
		]);
		expect(asked[3]!.body).toMatchObject({ revision: 32 });
	});

	it('gives up after a second conflict, leaving it for the next pass', async () => {
		const { control } = controlOf([
			app(30),
			refused(409, 'invalid_revision'),
			app(32),
			refused(409, 'invalid_revision'),
		]);
		await expect(control.release('geo', RELEASE)).rejects.toThrow(/invalid_revision/);
	});

	it('does not move a release backwards found on reading again', async () => {
		const { control } = controlOf([
			app(30),
			refused(409, 'invalid_revision'),
			app(32, { ...RELEASE, run: 10 }),
		]);
		expect(await control.release('geo', RELEASE)).toBe('not newer');
	});

	it('records nothing of an app control does not hold', async () => {
		const { asked, control } = controlOf([refused(404, 'no_such_resource')]);
		expect(await control.release('console', RELEASE)).toBe('no such app');
		expect(asked).toHaveLength(1);
	});

	it('says why when control cannot be reached', async () => {
		const { asked, control } = controlOf([new TypeError('fetch failed')]);
		await expect(control.release('geo', RELEASE)).rejects.toThrow(
			'control did not answer: fetch failed',
		);
		expect(asked).toHaveLength(1);
	});

	it('names the status of a failure on its side, or of an answer from in front of it', async () => {
		for (const [answer, said] of [
			[refused(503, 'service_unavailable'), '503 service_unavailable'],
			[new Response(null, { status: 405 }), '405'],
			[new Response('<html>Forbidden</html>', { status: 403 }), '403'],
		] as const) {
			const { control } = controlOf([app(30), answer]);
			// oxlint-disable-next-line no-await-in-loop -- one case at a time
			const failed = await control.release('geo', RELEASE).catch((error: Error) => error);
			expect(failed).toBeInstanceOf(Error);
			expect((failed as Error).message).toBe(`control answered ${said}`);
		}
	});

	it('names a refusal in its envelope by its code', async () => {
		const { asked, control } = controlOf([app(30), refused(401, 'invalid_token')]);
		await expect(control.release('geo', RELEASE)).rejects.toThrow(/invalid_token/);
		expect(asked).toHaveLength(2);
	});
});

describe('its address', () => {
	it('is configured by an address and a token alone', () => {
		expect(new Control(CONTROL, 'write').configured).toBe(true);
		expect(new Control(undefined, 'write').configured).toBe(false);
		expect(new Control(CONTROL, undefined).configured).toBe(false);
	});
});

/** A run of geo, console and site, written through control answering `answers` in order. */
async function flushed(answers: (Response | Error)[]) {
	const store = new Store(join(mkdtempSync(join(tmpdir(), 'deployer-')), 'deployer.db'));
	store.built('monoflake/platform', 9, COMMIT, ['console', 'geo', 'site']);
	const { control } = controlOf(answers);
	const logs: string[] = [];
	await new Releases({ store, control, log: (line) => logs.push(line) }).flush();
	return { store, logs };
}

describe('the releases', () => {
	it('settles an app control does not hold, and one it refuses, and keeps the rest', async () => {
		const { store, logs } = await flushed([
			refused(404, 'no_such_resource'),
			app(30),
			refused(400, 'invalid_body'),
			app(30),
			refused(401, 'invalid_token'),
		]);
		expect(store.unwritten().map((release) => release.app)).toEqual(['site']);
		expect(logs).toEqual([
			"deployer: console's release from run 9 of monoflake/platform is not recorded: control has no console",
			"deployer: geo's release from run 9 of monoflake/platform is not recorded: control answered invalid_body: invalid_body",
			"deployer: site's release from run 9 of monoflake/platform is not written yet: control answered invalid_token: invalid_token",
		]);
	});

	it('stops at control failing on its side, naming its status once', async () => {
		const { store, logs } = await flushed([app(30), refused(503, 'service_unavailable')]);
		expect(store.unwritten()).toHaveLength(3);
		expect(logs).toEqual([
			"deployer: console's release from run 9 of monoflake/platform is not written yet: control answered 503 service_unavailable",
		]);
	});

	it('stops at control not answering, the rest left for the next flush', async () => {
		const { store, logs } = await flushed([new TypeError('fetch failed')]);
		expect(store.unwritten()).toHaveLength(3);
		expect(logs).toHaveLength(1);
	});
});
