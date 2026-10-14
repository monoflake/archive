import { readFileSync, readdirSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import { describe, expect, it } from 'vitest';

/**
 * The runner moves into a workerd isolate unchanged, so what it ships uses web APIs alone. Its
 * tests may use node; nothing else here may. See RFC 23wgz, "The second slice, the executor".
 */
const SHIPPED = readdirSync(new URL('.', import.meta.url)).filter(
	(file) => file.endsWith('.ts') && !file.endsWith('.test.ts'),
);

const NODE_ONLY = [
	/\bfrom\s+['"]node:/,
	/\bimport\(\s*['"]node:/,
	/\brequire\(/,
	/\bprocess\./,
	/\bBuffer\b/,
	/\b__dirname\b/,
	/\bsetImmediate\(/,
];

describe('the runner', () => {
	it('ships at least the files it is made of', () => {
		expect(SHIPPED).toEqual(
			expect.arrayContaining(['replay.ts', 'step.ts', 'ledger.ts', 'config.ts']),
		);
	});

	// The executor runs it as written, with Node's types stripped and nothing compiled.
	it.each(SHIPPED)('%s is TypeScript Node can run by stripping its types', (file) => {
		const source = readFileSync(new URL(file, import.meta.url), 'utf8');
		expect(() => stripTypeScriptTypes(source, { mode: 'strip' })).not.toThrow();
	});

	it.each(SHIPPED)('%s uses web APIs alone', (file) => {
		const source = readFileSync(new URL(file, import.meta.url), 'utf8');
		for (const pattern of NODE_ONLY)
			expect(source, `${file} matches ${pattern}`).not.toMatch(pattern);
	});

	it.each(SHIPPED)('%s imports only itself and the names workerd provides', (file) => {
		const source = readFileSync(new URL(file, import.meta.url), 'utf8');
		for (const [, specifier] of source.matchAll(/\bfrom\s+['"]([^'"]+)['"]/g)) {
			expect(specifier, file).toMatch(/^(\.\/[\w-]+\.ts|cloudflare:workers|cloudflare:workflows)$/);
		}
	});
});
