#!/usr/bin/env node
//MISE description="Deploy control into its own Cloudflare account, its D1 migrations first: `deploy-control [--dry-run]`"
/**
 * The one way `apps/control/control` is deployed. Its account is not the platform's, so wrangler
 * is handed control's token and the one account it reaches, in its own environment alone, every
 * other Cloudflare variable left out. Neither is printed. The Worker's secrets are set apart from
 * this. See RFC o7eir.
 */
import { spawnSync } from 'node:child_process';
import { join } from 'node:path';
import { parseArgs } from 'node:util';
import * as v from 'valibot';
import { URLS } from '../../libs/sdk/src/index.ts';
import { ROOT } from './workspace.ts';

const APP = join(ROOT, 'apps/control/control');
const STEPS = [
	['wrangler', 'd1', 'migrations', 'apply', 'control', '--remote'],
	// `--env=` names the top level, as every deploy here does; see mise.toml, `deploy-cdn`.
	['wrangler', 'deploy', '--env='],
];

const Accounts = v.object({
	success: v.literal(true),
	result: v.array(v.object({ id: v.pipe(v.string(), v.nonEmpty()) })),
});

function fail(message: string): never {
	console.error(`deploy-control: ${message}`);
	process.exit(1);
}

/** The one account `token` reaches, by asking Cloudflare. */
async function account(token: string): Promise<string> {
	const response = await fetch(`${URLS.external.cloudflare.api}/accounts`, {
		headers: { Authorization: `Bearer ${token}` },
	});
	const parsed = v.safeParse(Accounts, await response.json().catch(() => undefined));
	if (!response.ok || !parsed.success) {
		fail(`Cloudflare did not list the token's accounts (${response.status})`);
	}
	const [only, ...more] = parsed.output.result;
	if (!only || more.length) {
		fail(`CLOUDFLARE_CONTROL_TOKEN reaches ${parsed.output.result.length} accounts, not one`);
	}
	return only.id;
}

/** This environment without anything that names a Cloudflare account or credential. */
function scrubbed(): NodeJS.ProcessEnv {
	return Object.fromEntries(
		Object.entries(process.env).filter(([name]) => !/^(CLOUDFLARE|CF)_/.test(name)),
	);
}

const { values } = parseArgs({ options: { 'dry-run': { type: 'boolean', short: 'n' } } });

if (values['dry-run']) {
	const token = process.env.CLOUDFLARE_CONTROL_TOKEN ? 'set' : 'not set, so a run would stop';
	console.log(
		`would ask Cloudflare for the one account CLOUDFLARE_CONTROL_TOKEN reaches (${token})`,
	);
	for (const step of STEPS) console.log(`would run, in ${APP}: pnpm exec ${step.join(' ')}`);
	process.exit(0);
}

const token = process.env.CLOUDFLARE_CONTROL_TOKEN;
if (!token) fail('CLOUDFLARE_CONTROL_TOKEN is not set');
const env = {
	...scrubbed(),
	CLOUDFLARE_API_TOKEN: token,
	CLOUDFLARE_ACCOUNT_ID: await account(token),
};
for (const step of STEPS) {
	const ran = spawnSync('pnpm', ['exec', ...step], { cwd: APP, env, stdio: 'inherit' });
	if (ran.status !== 0) fail(`${step.slice(0, 2).join(' ')} failed`);
}
console.log('deploy-control: control deployed');
