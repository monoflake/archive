import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { UPSERT, body, deploySources, widened } from './control.ts';

const NODE = 'aaaaaaaaaaaaaaaa';
const OTHER = 'bbbbbbbbbbbbbbbb';
const APP = 'cccccccccccccccc';

const declared = new Map([
	['tyo', { city: 'Tokyo', country: 'JP', continent: 'AS', place: 'Tokyo, Japan' }],
	['nrt', { city: 'Tokyo', country: 'JP', continent: 'AS' }],
]);
const nodes = {
	items: [
		{ id: NODE, name: 'tyo', revision: 3, spec: { tier: 'datacenter', place: 'Tokyo, Japan' } },
		{ id: OTHER, name: 'nrt', revision: 4, spec: { tier: 'datacenter' } },
	],
	revision: 9,
};
const apps = {
	items: [
		{
			id: APP,
			name: 'relay',
			revision: 8,
			spec: { placements: ['nrt', 'tyo'], display_name: 'Relay' },
		},
	],
	revision: 9,
};
const accounts = { items: [], revision: 9 };

describe("control's import", () => {
	it('drops a node its name and its place, and shows it as nodes.toml says', () => {
		const sent = body(nodes, apps, accounts, declared);
		expect(sent.nodes[0]).toEqual({
			id: NODE,
			name: null,
			revision: 3,
			spec: { tier: 'datacenter', city: 'Tokyo', country: 'JP', continent: 'AS' },
		});
	});

	it('places an app on node ids, in its order, and keeps the rest of its spec', () => {
		const [app] = body(nodes, apps, accounts, declared).apps;
		expect(app?.spec).toEqual({ placements: [OTHER, NODE], display_name: 'Relay' });
	});

	it('imports at the newest revision any list answered', () => {
		expect(body(nodes, apps, { items: [], revision: 11 }, declared).revision).toBe(11);
	});
});

describe("control's writers", () => {
	it('reads the sources host deploys from out of host_env.py, without running it', () => {
		const text = 'X = 1\nDEPLOY_SOURCES = "monoflake/infra=infra canmi21/cue=canmi"\n';
		expect(deploySources(text)).toEqual(['monoflake/infra', 'canmi21/cue']);
		expect(deploySources('DEPLOY_SOURCES = SOMETHING\n')).toBeUndefined();
	});

	it("widens a writer's sources, keeping its own first and each once", () => {
		expect(widened(' a/b  c/d a/b', ['c/d', 'e/f'])).toBe('a/b c/d e/f');
		expect(widened('', ['e/f'])).toBe('e/f');
	});

	it("writes an app's env as the database task does, byte for byte", () => {
		const task = readFileSync(join(import.meta.dirname, 'database'), 'utf8');
		expect(/^UPSERT = r"""([\s\S]*?)"""/m.exec(task)?.[1]).toBe(UPSERT);
	});
});
