import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { registries } from './config.ts';
import { type Release, Registry, Releases, newer } from './releases.ts';
import { Store } from './store.ts';

const COMMIT = 'c9a97660'.padEnd(40, '0');
const RELEASE: Release = { repository: 'monoflake/platform', run: 9, commit: COMMIT };

/** An app as `GET /v1/apps/{name}` answers it, at `revision`, released by `release`. */
function app(revision: number, release?: Partial<Release>) {
	const spec = release ? { placements: ['tyo'], release } : { placements: ['tyo'] };
	return Response.json({ status: 'success', data: { kind: 'app', name: 'geo', revision, spec } });
}

function refused(status: number, code: string) {
	return Response.json({ status: 'error', code, message: code }, { status });
}

/**
 * A registry over `answers`, taken in order by each request, recording what each asked; an answer
 * that is an Error is thrown, as a core that cannot be reached throws.
 */
function registryOf(answers: (Response | Error)[], cores = registries('100.64.0.1 100.64.0.2')) {
	const asked: { at: string; method: string; body?: unknown }[] = [];
	const fetch = async (url: string, init?: RequestInit) => {
		const { host, pathname } = new URL(url);
		const body = typeof init?.body === 'string' ? JSON.parse(init.body) : undefined;
		asked.push({ at: host, method: `${init?.method ?? 'GET'} ${pathname}`, body });
		expect(new Headers(init?.headers).get('authorization')).toBe('Bearer write');
		const answer = answers.shift();
		if (!answer || answer instanceof Error) throw answer ?? new Error('nothing more to answer');
		return answer;
	};
	return { asked, registry: new Registry(cores, 'write', fetch) };
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

describe('the registry', () => {
	it('writes a newer release held to the revision read, keeping the rest to the registry', async () => {
		const { asked, registry } = registryOf([
			app(30, { ...RELEASE, run: 8 }),
			Response.json({ status: 'success', data: { revision: 31 } }),
		]);
		expect(await registry.release('geo', RELEASE)).toBe('written');
		expect(asked).toEqual([
			{ at: '100.64.0.1:12013', method: 'GET /v1/apps/geo', body: undefined },
			{
				at: '100.64.0.1:12013',
				method: 'PATCH /v1/apps/geo',
				body: { by: 'deployer', revision: 30, spec: { release: RELEASE } },
			},
		]);
	});

	it('writes nothing over the same run or a newer one', async () => {
		for (const run of [9, 10]) {
			const { asked, registry } = registryOf([app(30, { ...RELEASE, run })]);
			// oxlint-disable-next-line no-await-in-loop -- one case at a time
			expect(await registry.release('geo', RELEASE)).toBe('not newer');
			expect(asked).toHaveLength(1);
		}
	});

	it('reads again and writes once more when another change got in first', async () => {
		const { asked, registry } = registryOf([
			app(30),
			refused(409, 'invalid_revision'),
			app(32, { ...RELEASE, run: 8 }),
			Response.json({ status: 'success', data: { revision: 33 } }),
		]);
		expect(await registry.release('geo', RELEASE)).toBe('written');
		expect(asked.map(({ method }) => method)).toEqual([
			'GET /v1/apps/geo',
			'PATCH /v1/apps/geo',
			'GET /v1/apps/geo',
			'PATCH /v1/apps/geo',
		]);
		expect(asked[3]!.body).toMatchObject({ revision: 32 });
	});

	it('gives up after a second conflict, leaving it for the next pass', async () => {
		const { registry } = registryOf([
			app(30),
			refused(409, 'invalid_revision'),
			app(32),
			refused(409, 'invalid_revision'),
		]);
		await expect(registry.release('geo', RELEASE)).rejects.toThrow(/invalid_revision/);
	});

	it('does not move a release backwards found on reading again', async () => {
		const { registry } = registryOf([
			app(30),
			refused(409, 'invalid_revision'),
			app(32, { ...RELEASE, run: 10 }),
		]);
		expect(await registry.release('geo', RELEASE)).toBe('not newer');
	});

	it('records nothing of an app the registry does not hold', async () => {
		const { asked, registry } = registryOf([refused(404, 'no_such_resource')]);
		expect(await registry.release('console', RELEASE)).toBe('no such app');
		expect(asked).toHaveLength(1);
	});

	it('asks the next core when one cannot be reached or fails, and that one first after', async () => {
		const { asked, registry } = registryOf([
			new TypeError('fetch failed'),
			app(30),
			refused(503, 'store_unavailable'),
			Response.json({ status: 'success', data: { revision: 31 } }),
			app(31, RELEASE),
		]);
		expect(await registry.release('geo', RELEASE)).toBe('written');
		expect(await registry.release('geo', RELEASE)).toBe('not newer');
		expect(asked.map(({ at }) => at)).toEqual([
			'100.64.0.1:12013',
			'100.64.0.2:12013',
			'100.64.0.2:12013',
			'100.64.0.1:12013',
			'100.64.0.1:12013',
		]);
	});

	it('says why when no core answers', async () => {
		const { registry } = registryOf([new TypeError('fetch failed'), new TypeError('fetch failed')]);
		await expect(registry.release('geo', RELEASE)).rejects.toThrow(
			'no registry answered: 100.64.0.2:12013: fetch failed',
		);
	});

	it('does not ask the next core over a refusal, which every core would give', async () => {
		const { asked, registry } = registryOf([app(30), refused(401, 'invalid_token')]);
		await expect(registry.release('geo', RELEASE)).rejects.toThrow(/invalid_token/);
		expect(asked).toHaveLength(2);
	});
});

describe('registries', () => {
	it('gives each address the registry port where it names none', () => {
		expect(registries(' 100.64.0.1\n100.64.0.2:9 ')).toEqual(['100.64.0.1:12013', '100.64.0.2:9']);
		expect(registries('')).toEqual([]);
	});
});

/** A run of geo, console and site, written through a registry answering `answers` in order. */
async function flushed(answers: (Response | Error)[]) {
	const store = new Store(join(mkdtempSync(join(tmpdir(), 'deployer-')), 'deployer.db'));
	store.built('monoflake/platform', 9, COMMIT, ['console', 'geo', 'site']);
	const { registry } = registryOf(answers);
	const logs: string[] = [];
	await new Releases({ store, registry, log: (line) => logs.push(line) }).flush();
	return { store, logs };
}

describe('the releases', () => {
	it('settles an app the registry does not hold, and one it refuses, and keeps the rest', async () => {
		const { store, logs } = await flushed([
			refused(404, 'no_such_resource'),
			app(30),
			refused(400, 'invalid_body'),
			app(30),
			refused(401, 'invalid_token'),
		]);
		expect(store.unwritten().map((release) => release.app)).toEqual(['site']);
		expect(logs).toEqual([
			"deployer: console's release from run 9 of monoflake/platform is not recorded: the registry has no console",
			"deployer: geo's release from run 9 of monoflake/platform is not recorded: the registry at 100.64.0.1:12013 answered invalid_body: invalid_body",
			"deployer: site's release from run 9 of monoflake/platform is not written yet: the registry at 100.64.0.1:12013 answered invalid_token: invalid_token",
		]);
	});

	it('stops at a registry that does not answer, the rest left for the next flush', async () => {
		const down = new TypeError('fetch failed');
		const { store, logs } = await flushed([down, down]);
		expect(store.unwritten()).toHaveLength(3);
		expect(logs).toHaveLength(1);
	});
});
