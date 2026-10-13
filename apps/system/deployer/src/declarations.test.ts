import { describe, expect, it } from 'vitest';
import { isManual, manualIn } from './declarations.ts';
import { zipOf } from './fixtures.ts';

describe('isManual', () => {
	it('reads `rollout = "manual"` among the top-level keys', () => {
		expect(isManual('name = "database"\nrollout = "manual"\n')).toBe(true);
		expect(isManual('rollout="manual" # see the spec\nname = "quorum"\n')).toBe(true);
	});

	it('passes over anything else, a key of a table among it', () => {
		expect(isManual('name = "geo"\n')).toBe(false);
		expect(isManual('rollout = "auto"\n')).toBe(false);
		expect(isManual('# rollout = "manual"\nname = "geo"\n')).toBe(false);
		expect(isManual('name = "geo"\n[container]\nrollout = "manual"\n')).toBe(false);
	});
});

describe('manualIn', () => {
	it('names each app its declarations say is rolled out by hand', () => {
		const zip = zipOf([
			{ name: 'database.toml', data: 'name = "database"\nrollout = "manual"\n' },
			{ name: 'geo.toml', data: 'name = "geo"\n' },
			{ name: 'quorum.toml', data: 'name = "quorum"\nrollout = "manual"\n' },
		]);
		expect(manualIn(zip)).toEqual(['database', 'quorum']);
	});

	it('refuses an entry the Deploy workflow never writes', () => {
		for (const name of ['geo/service.toml', '../geo.toml', 'geo.json', 'Geo.toml']) {
			expect(() => manualIn(zipOf([{ name, data: '' }]))).toThrow(/is not <app>\.toml|plain/);
		}
	});
});
