import { URLS } from './index.ts';

/**
 * The Python mirror of the URL map, `src/urls.py`, which the platform's Python tasks import: every
 * string as a constant named by its path in capitals, the names `lib.rs` gives them, so
 * `external.artifacts` is `EXTERNAL_ARTIFACTS`. `mise run urls` rewrites it and `python.test.ts`
 * fails when it no longer matches this render. See the workspace's spec/addresses.md, "Every URL
 * is declared once".
 */
export function pythonUrlMap(): string {
	const pairs: Array<[name: string, value: string]> = [];
	walk(URLS, [], pairs);
	return [
		'"""@generated from libs/sdk/src/index.ts by `mise run urls`; do not edit.',
		"One URL map for every language -- see the workspace's spec/addresses.md.",
		'"""',
		'',
		...pairs.map(([name, value]) => `${name} = ${JSON.stringify(value)}`),
		'',
	].join('\n');
}

function walk(value: unknown, path: string[], out: Array<[string, string]>): void {
	if (typeof value === 'string') {
		const name = path
			.map((segment) => segment.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toUpperCase())
			.join('_');
		out.push([name, value]);
		return;
	}
	// Only strings become constants, as in the Rust mirror.
	if (typeof value !== 'object' || value === null) return;
	for (const [key, child] of Object.entries(value as Record<string, unknown>)) {
		walk(child, [...path, key], out);
	}
}
