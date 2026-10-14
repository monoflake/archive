import { defineConfig } from 'vitest/config';

// The end-to-end tests alone, against the real ledger: `pnpm run e2e`, never the workspace's
// `vitest run`, which runs where no ledger is. See test/e2e/world.ts.
export default defineConfig({
	test: {
		include: ['test/**/*.e2e.test.ts'],
		testTimeout: 30_000,
		hookTimeout: 60_000,
	},
});
