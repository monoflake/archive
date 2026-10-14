import { describe, expect, test } from 'vitest';
import type * as Verify from '../../../../.mise/tasks/change/verify.ts';
import { shared } from './workspace.ts';
import { CONFIG } from './verify.ts';

// verify's choice of gates for this repository's own files.
const { plan } = (await import(shared('change/verify.ts'))) as typeof Verify;
const gates = (path: string) => plan(CONFIG, [path]).gates;

describe("a crate's scripts", () => {
	test('are type-checked, linted and tested, though no package owns them', () => {
		for (const path of [
			'apps/data/registry/scripts/secret.ts',
			'apps/data/registry/scripts/platform.test.ts',
		]) {
			expect(gates(path), path).toEqual(expect.arrayContaining(['check', 'lint', 'test']));
		}
	});

	test("are not the crate's own source, which runs the Rust gates alone", () => {
		const rust = gates('apps/data/registry/src/main.rs');
		expect(rust).toEqual(expect.arrayContaining(['lint-rust', 'test-rust']));
		expect(rust).not.toContain('test');
	});
});
