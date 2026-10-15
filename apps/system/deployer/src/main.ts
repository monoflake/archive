/**
 * The deployer as host runs it: its configuration from the environment, its records in `/data`, and
 * one HTTP server. It starts with nothing configured and refuses every run until it is. See
 * spec/architecture/deployer.md.
 */
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { serve } from '@hono/node-server';
import { Bucket } from './bucket.ts';
import { CatchUp, every } from './catch-up.ts';
import { bucketMissing, configOf, tokenFor } from './config.ts';
import { Deployer } from './deploy.ts';
import { GitHub } from './github.ts';
import { Control, Releases } from './releases.ts';
import { handle } from './server.ts';
import { Store } from './store.ts';
import { run } from './wrangler.ts';

const config = configOf(process.env);
mkdirSync(config.data, { recursive: true });
const store = new Store(join(config.data, 'deployer.db'));
const github = new GitHub((repository) => tokenFor(repository, process.env));
// The run is GitHub's to vouch for either way; only its artifacts move to the bucket, RFC q28g5.
const source = config.artifacts ? new Bucket(config.artifacts, github) : github;
const control = new Control(config.control, config.controlToken);
const releases = new Releases({ store, control });
const deployer = new Deployer({ config, store, github: source, wrangler: run, releases });

if (config.sources.length === 0) console.log('deployer: DEPLOY_SOURCES names no repository');
if (config.owners.size === 0) console.log('deployer: WORKER_OWNERS names no Worker');
const missing = bucketMissing(process.env);
if (missing.length > 0) {
	console.log(`deployer: ${missing.join(', ')} not set, so artifacts are read from GitHub`);
}
if (!control.configured) {
	console.log('deployer: CONTROL or CONTROL_WRITE_TOKEN is not set, so no release is recorded yet');
}

serve({
	port: config.port,
	hostname: '::',
	fetch: (request) =>
		handle(request, {
			token: config.token,
			readToken: config.readToken,
			owned: (worker) => config.owners.has(worker),
			notice: (notice) => deployer.notice(notice),
			rollback: (rollback) => deployer.rollback(rollback),
			deploys: (limit, before) => store.list(limit, before),
		}),
});
console.log(`deployer: listening on ${config.port}${config.dry ? ', every deploy dry' : ''}`);

// A run whose notice never came is taken anyway, at start and every 15 minutes; see catch-up.ts.
const catchUp = new CatchUp({ sources: config.sources, store, github: source, deployer, releases });
every(() => catchUp.pass());
