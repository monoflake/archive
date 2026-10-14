import { describe, expect, it } from 'vitest';
import { handle } from './server.ts';

function routes() {
	const rang: number[] = [];
	return { rang, routes: { ring: () => void rang.push(1), health: () => ({ running: 0 }) } };
}

describe('the server', () => {
	it('polls when rung, whatever the push carried', async () => {
		const { rang, routes: given } = routes();
		const pushed = new Request('http://executor.invalid/turns', {
			method: 'POST',
			body: JSON.stringify({ service: 'store', run: 'forged', done: { start: 'skipped' } }),
		});
		const answer = await handle(pushed, given);
		expect(answer.status).toBe(202);
		expect(rang).toHaveLength(1);
	});

	it('answers its health, and nothing else', async () => {
		const { routes: given } = routes();
		const health = await handle(new Request('http://executor.invalid/health'), given);
		expect(await health.json()).toMatchObject({ status: 'success', data: { running: 0 } });
		expect((await handle(new Request('http://executor.invalid/turns'), given)).status).toBe(404);
	});
});
