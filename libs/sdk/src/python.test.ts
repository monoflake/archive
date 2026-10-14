import { readFileSync } from 'node:fs';
import { expect, it } from 'vitest';
import { pythonUrlMap } from './python.ts';

it('keeps the committed Python mirror in step with the map', () => {
	const committed = readFileSync(new URL('./urls.py', import.meta.url), 'utf8');
	// A mismatch means the map changed without `mise run urls` -- regenerate rather than edit the
	// Python file, which carries a do-not-edit header for this reason.
	expect(committed).toBe(pythonUrlMap());
});

/** Every constant's name `pattern` finds in `text`, in order. */
function names(text: string, pattern: RegExp): Array<string | undefined> {
	return [...text.matchAll(pattern)].map((match) => match[1]);
}

it('names each address as the Rust mirror does', () => {
	const rust = readFileSync(new URL('./lib.rs', import.meta.url), 'utf8');
	expect(names(pythonUrlMap(), /^([A-Z0-9_]+) = /gm)).toEqual(
		names(rust, /^pub const ([A-Z0-9_]+): &str/gm),
	);
});
