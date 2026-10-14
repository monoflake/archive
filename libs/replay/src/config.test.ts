import { describe, expect, it } from 'vitest';
import { CALLBACK_LIMIT_MS, milliseconds, wire } from './config.ts';

describe('milliseconds', () => {
	it('reads a number as milliseconds and a string as Workflows spells it', () => {
		expect(milliseconds(1500)).toBe(1500);
		expect(milliseconds('1 second')).toBe(1_000);
		expect(milliseconds('10 seconds')).toBe(10_000);
		expect(milliseconds('2.5 minutes')).toBe(150_000);
		expect(milliseconds('1 hour')).toBe(3_600_000);
		expect(milliseconds('3 days')).toBe(259_200_000);
		expect(milliseconds('1 week')).toBe(604_800_000);
	});

	it('refuses what is not a duration', () => {
		expect(() => milliseconds(-1)).toThrow(TypeError);
		expect(() => milliseconds(Number.NaN)).toThrow(TypeError);
		expect(() => milliseconds('soon' as never)).toThrow(TypeError);
		expect(() => milliseconds('1 fortnight' as never)).toThrow(TypeError);
	});
});

describe('wire', () => {
	it('fills what the code left out with Workflows defaults', () => {
		expect(wire()).toEqual({
			retries: { limit: 5, delay_ms: 10_000, backoff: 'exponential' },
			timeout_ms: CALLBACK_LIMIT_MS,
		});
		expect(
			wire({ retries: { limit: 2, delay: '1 minute', backoff: 'linear' }, timeout: 30_000 }),
		).toEqual({
			retries: { limit: 2, delay_ms: 60_000, backoff: 'linear' },
			timeout_ms: 30_000,
		});
	});

	it('holds a callback to five minutes, so the default ten minutes reads as five', () => {
		expect(CALLBACK_LIMIT_MS).toBe(300_000);
		expect(wire({ timeout: '10 minutes' }).timeout_ms).toBe(300_000);
		expect(wire({ timeout: '1 hour' }).timeout_ms).toBe(300_000);
		expect(wire({ timeout: '1 minute' }).timeout_ms).toBe(60_000);
	});

	it('refuses a retry limit Workflows would and a delay given as a function', () => {
		expect(() => wire({ retries: { limit: 10_001, delay: 1 } })).toThrow(TypeError);
		expect(() => wire({ retries: { limit: 1.5, delay: 1 } })).toThrow(TypeError);
		expect(() => wire({ retries: { limit: 1, delay: (() => 1) as never } })).toThrow(TypeError);
	});
});
