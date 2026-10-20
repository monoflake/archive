import { describe, expect, it } from 'vitest';
import { CAPACITY, PORT, capacityOf, configOf, defaultBindings, listenOf } from './config.ts';

describe('the configuration', () => {
	it('listens where LISTEN says, as every platform service reads it', () => {
		expect(listenOf(undefined)).toEqual({ host: '0.0.0.0', port: PORT });
		expect(listenOf('127.0.0.1:9000')).toEqual({ host: '127.0.0.1', port: 9000 });
		expect(listenOf('[::]:12030')).toEqual({ host: '::', port: 12030 });
		expect(listenOf('nonsense')).toEqual({ host: '0.0.0.0', port: PORT });
	});

	it('holds as many leases as CAPACITY says, at least one', () => {
		expect(capacityOf('2')).toBe(2);
		for (const wrong of [undefined, '0', '-1', '1.5', 'many'])
			expect(capacityOf(wrong)).toBe(CAPACITY);
	});

	it('reaches the ledger and is rung on the private side unless told otherwise', () => {
		const config = configOf({ NODE: 'tyo', NODE_ID: '0000000000000001' });
		expect(config.node).toBe('0000000000000001');
		expect(config.ledger).toMatch(/\/ledger$/);
		expect(config.pushUrl).toMatch(/\/executor\/turns$/);
		expect(config.bindings).toEqual(defaultBindings());
		expect(configOf({ LEDGER_URL: 'http://ledger.invalid:12010' }).ledger).toBe(
			'http://ledger.invalid:12010',
		);
	});

	it('adds and replaces bindings from BINDINGS', () => {
		const { bindings } = configOf({
			BINDINGS: 'STORE=http://store.invalid, LEDGER=http://ledger.invalid',
		});
		expect(bindings.STORE).toBe('http://store.invalid');
		expect(bindings.LEDGER).toBe('http://ledger.invalid');
		expect(configOf({}).bindings).toEqual({ LEDGER: defaultBindings().LEDGER });
	});

	it('leaves NODE_ID unset when it is, so the executor refuses to start', () => {
		expect(configOf({}).node).toBeUndefined();
		expect(configOf({ NODE: 'tyo' }).node).toBeUndefined();
	});
});
