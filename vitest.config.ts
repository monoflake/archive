import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitest/config';

export default defineConfig({
	test: {
		projects: [
			{
				/**
				 * `apps/delivery/cdn` imports its codecs' `.wasm` files directly, which wrangler
				 * substitutes at bundle time and node does not. An empty stub is safe because a codec
				 * initializes lazily: a path that does not encode or decode never touches it.
				 */
				plugins: [
					{
						name: 'stub-wasm',
						// Ahead of Vite's own resolver, which otherwise hands the bytes to the JS loader and
						// fails while reading them as source.
						enforce: 'pre' as const,
						resolveId(id) {
							return id.endsWith('.wasm') ? '\0stub-wasm' : null;
						},
						load(id) {
							return id === '\0stub-wasm' ? 'export default {};' : null;
						},
					},
				],
				// The names workerd provides, which the runner's own modules stand in for under Node, as
				// tsconfig.json maps them for the type checker. See RFC 23wgz.
				resolve: {
					alias: [
						{
							find: /^cloudflare:(workers|workflows)$/,
							replacement: fileURLToPath(new URL('libs/replay/src/cloudflare.ts', import.meta.url)),
						},
					],
				},
				test: {
					name: 'node',
					include: ['{apps,libs}/**/*.test.ts', '.mise/tasks/*.test.ts'],
					// The executor's end-to-end tests need a ledger; they run by its own `pnpm run e2e`.
					exclude: ['**/node_modules/**', '**/*.svelte.test.ts', '**/*.e2e.test.ts'],
				},
			},
		],
	},
});
