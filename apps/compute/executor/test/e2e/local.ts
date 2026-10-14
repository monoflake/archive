/**
 * The end-to-end tests on this machine, as CI runs them: a throwaway Postgres 18 in Docker, the
 * image the cluster runs, and the ledger built from this tree. `pnpm run e2e:local`; the container
 * is removed however the tests end. See .mise/tasks/test-sql, which the SQL tests run by.
 */

import { execFileSync, spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

const root = new URL('../../../../../', import.meta.url);
const dockerfile = readFileSync(new URL('apps/data/database/Dockerfile', root), 'utf8');
const image = /^FROM (postgres:\S+)/m.exec(dockerfile)?.[1];
if (!image) throw new Error('no postgres image in apps/data/database/Dockerfile');

/** The ledger's binary, built here, where cargo says it put it. */
function ledger(): string {
	const built = execFileSync(
		'cargo',
		[
			'build',
			'-p',
			'ledger-service',
			'--bin',
			'ledger',
			'--message-format=json-render-diagnostics',
		],
		{ cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] },
	);
	for (const line of built.split('\n').toReversed()) {
		const message = line
			? (JSON.parse(line) as { target?: { name?: string }; executable?: string })
			: {};
		if (message.target?.name === 'ledger' && message.executable) return message.executable;
	}
	throw new Error('cargo built no ledger binary');
}

const name = `e2e-${process.pid}`;
const docker = (...args: string[]) =>
	execFileSync('docker', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] }).trim();
let status = 1;
try {
	docker(
		'run',
		'-d',
		'--name',
		name,
		'-e',
		'POSTGRES_PASSWORD=test',
		'-p',
		'127.0.0.1::5432',
		image,
	);
	const port = docker('port', name, '5432/tcp').split('\n')[0]?.split(':').pop();
	const deadline = Date.now() + 60_000;
	while (
		spawnSync('docker', ['exec', name, 'pg_isready', '-U', 'postgres', '-h', '127.0.0.1'])
			.status !== 0
	) {
		if (Date.now() > deadline) throw new Error('Postgres did not start in 60 seconds');
		execFileSync('sleep', ['1']);
	}
	const env = {
		...process.env,
		LEDGER_TEST_DATABASE_URL: `postgresql://postgres:test@127.0.0.1:${port}/postgres`,
		LEDGER_BIN: ledger(),
	};
	status =
		spawnSync('pnpm', ['run', 'e2e'], {
			env,
			stdio: 'inherit',
			cwd: new URL('../../', import.meta.url),
		}).status ?? 1;
} finally {
	spawnSync('docker', ['rm', '-f', '-v', name], { stdio: 'ignore' });
}
process.exit(status);
