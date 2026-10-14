/**
 * The executor bundled for its image, into dist/: the driver, `main.mjs`; the thread's entry and
 * the turn it runs, `thread.mjs` and `turn.mjs`; the stand-in for `cloudflare:*`,
 * `cloudflare.mjs`; and each workflow, `workflows/<service>/<name>.mjs`. `cloudflare:*` stays a
 * name in every bundle, so the hook resolves it to the one stand-in and a workflow's
 * `NonRetryableError` is the runner's. `node scripts/bundle.ts`.
 */

import { readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { build, type Plugin } from 'esbuild';

const app = fileURLToPath(new URL('..', import.meta.url));

/** src/files.ts as the bundle lays the files out: each beside the driver's own. */
const LAYOUT = `
export const THREAD = new URL('./thread.mjs', import.meta.url);
export const TURN = new URL('./turn.mjs', import.meta.url);
export const SHIM = new URL('./cloudflare.mjs', import.meta.url).href;
export const WORKFLOWS = new URL('./workflows/', import.meta.url);
export const EXTENSION = '.mjs';
`;

const layout: Plugin = {
	name: 'layout',
	setup(bundle) {
		bundle.onLoad({ filter: /[\\/]src[\\/]files\.ts$/ }, () => ({
			contents: LAYOUT,
			loader: 'ts',
		}));
	},
};

/** Every workflow under workflows/, the registry aside, as an entry named by its path. */
function workflows(): Record<string, string> {
	const entries: Record<string, string> = {};
	for (const service of readdirSync(`${app}workflows`, { withFileTypes: true })) {
		if (!service.isDirectory()) continue;
		for (const file of readdirSync(`${app}workflows/${service.name}`)) {
			if (!file.endsWith('.ts') || file.endsWith('.test.ts')) continue;
			const name = `workflows/${service.name}/${file.slice(0, -'.ts'.length)}`;
			entries[name] = `${app}${name}.ts`;
		}
	}
	return entries;
}

await build({
	absWorkingDir: app,
	entryPoints: {
		main: 'src/main.ts',
		thread: 'src/thread.ts',
		turn: 'src/turn.ts',
		cloudflare: fileURLToPath(import.meta.resolve('@monoflake/replay/cloudflare')),
		...workflows(),
	},
	bundle: true,
	platform: 'node',
	format: 'esm',
	target: 'node26',
	outdir: 'dist',
	outExtension: { '.js': '.mjs' },
	external: ['cloudflare:workers', 'cloudflare:workflows'],
	plugins: [layout],
	logLevel: 'warning',
});
