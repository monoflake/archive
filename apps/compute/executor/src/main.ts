/**
 * The executor as host runs it: its configuration from the environment, a driver polling the
 * ledger, and one HTTP server for the doorbell and health. See spec/architecture/executor.md,
 * "Configuration" and "Stopping".
 */

import { serve } from '@hono/node-server';
import { WORKFLOWS } from '../workflows/index.ts';
import { configOf } from './config.ts';
import { Driver } from './driver.ts';
import { ledgerAt } from './ledger.ts';
import { handle } from './server.ts';
import { threads } from './turns.ts';

/** How long a stopping executor lets its turns run, inside the 20 seconds host gives it. */
const STOPPING_MS = 15_000;

const config = configOf(process.env);
if (!config.node) {
	console.log('executor: NODE is not set; host sets it on every container');
	process.exit(1);
}
const node = config.node;
const driver = new Driver({
	node,
	capacity: config.capacity,
	pushUrl: config.pushUrl,
	workflows: Object.keys(WORKFLOWS),
	ledger: ledgerAt(config.ledger),
	start: threads({ node, ledger: config.ledger, bindings: config.bindings, workflows: WORKFLOWS }),
});

const server = serve({
	port: config.listen.port,
	hostname: config.listen.host,
	fetch: (request) =>
		handle(request, {
			ring: () => void driver.ring(),
			health: () => ({
				node,
				capacity: config.capacity,
				running: driver.running,
				polled_at: driver.polledAt?.toISOString() ?? null,
			}),
		}),
});
console.log(
	`executor: ${node} listening on ${config.listen.host}:${config.listen.port}, capacity ` +
		`${config.capacity}, running ${Object.keys(WORKFLOWS).join(', ')}`,
);
driver.start();

let stopping = false;
const stop = async () => {
	if (stopping) return;
	stopping = true;
	console.log('executor: stopping');
	server.close();
	await driver.stop(STOPPING_MS);
	process.exit(0);
};
process.on('SIGTERM', () => void stop());
process.on('SIGINT', () => void stop());
