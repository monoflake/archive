import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, test } from 'vitest';
import type * as Deployable from '../../../../.mise/tasks/change/deployable.ts';
import type * as Graph from '../../../../.mise/tasks/change/graph.ts';
import { LAYOUT, ROOT, shared } from './workspace.ts';

// deployable's choice of images, and its split between images to build and declarations to apply
// alone, against this repository's own apps.
const engine = (await import(shared('change/deployable.ts'))) as typeof Deployable;
const { rustGraph } = (await import(shared('change/graph.ts'))) as typeof Graph;
const DECLARED = engine.apps(LAYOUT);

type Revisions = [Deployable.Readable, Deployable.Readable];

/** `text` with the first `count` of `from` replaced by `to`. */
function firstOf(text: string, from: string, to: string, count: number): string {
	let result = text;
	for (let at = 0, n = 0; n < count; n++) {
		const found = result.indexOf(from, at);
		if (found < 0) break;
		result = result.slice(0, found) + to + result.slice(found + from.length);
		at = found + to.length;
	}
	return result;
}

/** A revision holding `texts`, path to text, and the working copy's text of any other path. */
const revision = (texts: Record<string, string>): Deployable.Readable => ({
	read: (path) => texts[path] ?? new engine.Revision(ROOT).read(path),
});

/** The images `files` need rebuilt, and the declarations they leave to apply alone. */
function split(files: string[], revisions?: Revisions): [string[], string[]] {
	const images = engine.needed(LAYOUT, files, DECLARED, revisions);
	return [images, engine.redeclared(LAYOUT, files, DECLARED, images, revisions)];
}

/** `path` changed by `change`, a function of its text, as the revisions to compare. */
function edited(path: string, change: (text: string) => string): Revisions {
	const before = readFileSync(join(ROOT, path), 'utf8');
	return [revision({ [path]: before }), revision({ [path]: change(before) })];
}

/** The images a change to `path` needs, by `change`, a function of its text; given none, with no
 * revisions to read, as `--files` asks. */
function rebuilt(path: string, change?: (text: string) => string): string[] {
	return split([path], change && edited(path, change))[0];
}

describe('declarations', () => {
	test('a changed declaration alone is applied without an image', () => {
		expect(split(['apps/compute/geo/service.toml'])).toEqual([[], ['geo']]);
	});

	test('a declaration beside anything else of its app is built', () => {
		expect(split(['apps/compute/geo/service.toml', 'apps/compute/geo/Dockerfile'])).toEqual([
			['geo'],
			[],
		]);
	});

	test('a declaration of an app built whole is applied alone too', () => {
		// deployer's Dockerfile copies its whole directory; its declaration is still host's to read.
		expect(split(['apps/system/deployer/service.toml'])).toEqual([[], ['deployer']]);
	});

	test("only the app's own declaration is one", () => {
		expect(engine.declaration(LAYOUT, 'apps/compute/geo/service.toml')).toBe(true);
		expect(engine.declaration(LAYOUT, 'apps/compute/geo/src/service.toml')).toBe(false);
		expect(engine.declaration(LAYOUT, 'apps/compute/geo/Dockerfile')).toBe(false);
	});

	test('a comment in a declaration is neither', () => {
		const declaration = 'apps/compute/geo/service.toml';
		expect(
			split(
				[declaration],
				edited(declaration, (text) => `# Why.\n${text}`),
			),
		).toEqual([[], []]);
	});

	test('a declaration a test module of its own includes is applied alone', () => {
		// secrets' peers/tests.rs reads its declaration; its binary does not.
		expect(split(['apps/data/secrets/service.toml'])).toEqual([[], ['secrets']]);
	});
});

const GEO = 'apps/compute/geo/src/main.rs';

describe('fingerprints', () => {
	test('a comment in Rust is no change', () => {
		expect(
			rebuilt(GEO, (text) => `// A note.\n${firstOf(text, '\n', '\n/* and */\n', 3)}`),
		).toEqual([]);
	});

	test('a reformat is no change', () => {
		expect(
			rebuilt(GEO, (text) => text.replaceAll('\t', '    ').replaceAll(' {\n', '\n{\n')),
		).toEqual([]);
	});

	test('a changed token is a rebuild', () => {
		expect(rebuilt(GEO, (text) => `${text}\nfn another() {}\n`)).toEqual(['geo']);
		expect(rebuilt(GEO)).toEqual(['geo']);
	});

	test('prose and tests build nothing', () => {
		for (const path of [
			'apps/compute/geo/README.md',
			'apps/data/primary/tests/proxy.rs',
			'LICENSE',
		]) {
			expect(rebuilt(path), path).toEqual([]);
		}
	});

	test('a value in a declaration is applied without a rebuild', () => {
		const declaration = 'apps/compute/geo/service.toml';
		expect(
			split(
				[declaration],
				edited(declaration, (text: string) =>
					text.replace('name = "geo"', 'name = "geo"\nhealth_timeout = 9'),
				),
			),
		).toEqual([[], ['geo']]);
	});

	test('a comment in a Dockerfile is no change', () => {
		const dockerfile = 'apps/compute/geo/Dockerfile';
		expect(rebuilt(dockerfile, (text) => `${text}\n# Done.\n`)).toEqual([]);
		expect(
			rebuilt(dockerfile, (text) => text.replace('FROM ', 'FROM --platform=$BUILDPLATFORM ')),
		).toEqual(['geo']);
	});

	test('the build is every image unless only its comments moved', () => {
		expect(rebuilt('.dockerignore', (text) => `${text}# More.\n`)).toEqual([]);
		expect(rebuilt('.dockerignore', (text) => `${text}**/cache\n`)).toEqual(
			[...DECLARED.keys()].toSorted(),
		);
	});

	test("a binary's directory holds nothing else its image is built from", () => {
		// apt's units and apk's door are installed on the node, never built into the image.
		for (const path of [
			'apps/system/apt/units/apt-nightly-update.service',
			'apps/system/apk/door/apk-door.sh',
		]) {
			expect(rebuilt(path), path).toEqual([]);
		}
		// What a binary includes is in it.
		expect(rebuilt('apps/observe/probe/checks.toml')).toEqual(['probe']);
		// deployer copies its whole directory, so anything there is its image's.
		expect(engine.cargoBuilt(LAYOUT, DECLARED, rustGraph(ROOT).directories)).not.toContain(
			'deployer',
		);
		expect(rebuilt('apps/system/deployer/src/extra.ts')).toEqual(['deployer']);
	});
});

describe('a Node image', () => {
	test('is rebuilt when a package it imports changes, as it bundles that package', () => {
		// The executor imports the workflow runner; the deployer does not.
		expect(rebuilt('libs/replay/src/cloudflare.ts')).toEqual(['executor']);
		// Both import the sdk.
		expect(rebuilt('libs/sdk/symlink/src/index.ts')).toEqual(['deployer', 'executor']);
	});

	test("is not rebuilt for a package's prose, or for a package no image imports", () => {
		expect(rebuilt('libs/replay/README.md')).toEqual([]);
		expect(rebuilt('libs/probe/package.json')).toEqual([]);
	});

	test("is chosen only where it is built from its package, never a crate's binary", () => {
		// geo has no package, and the probe's binary is a crate's whatever it imports.
		expect(rebuilt('libs/sdk/symlink/src/index.ts')).not.toContain('geo');
		expect(rebuilt('libs/sdk/symlink/src/index.ts')).not.toContain('probe');
	});
});
