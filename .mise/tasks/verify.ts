#!/usr/bin/env node
//MISE description="Every gate a change reaches: `verify [--all] [--since REV] [--dry-run] [--files ...]`"
/**
 * The gates a change has to pass, chosen by what it touched: the workspace's engine, with this
 * repository's gates. See the workspace's spec/architecture/repos.md, "verify runs what a change
 * reaches".
 */
import type * as Verify from '../../../../.mise/tasks/change/verify.ts';
import { LAYOUT, shared } from './workspace.ts';

const { isDeclaration, main } = (await import(shared('change/verify.ts'))) as typeof Verify;

export const CONFIG: Verify.Config = {
	layout: LAYOUT,
	always: [
		'secrets',
		'refs',
		'comments',
		'shape',
		'rules',
		'check-scopes',
		'check-receivers',
		'check-catalog',
	],
	// tsc reads three programs, vitest one suite, oxlint the tree.
	node: ['check', 'lint', 'test'],
	packageGates: { '@monoflake/probe': ['check-probe'] },
	rust: ['lint-rust', 'test-rust'],
	tasks: ['test-tasks'],
	// The generated half of the address map, whose only check is a TypeScript test.
	nodeCheckedRust: ['libs/sdk/src/lib.rs'],
	// Every app's declaration is read by the gateway's scope table, whichever app it sits in, and so
	// are the ones registered from elsewhere.
	reaches: (path) =>
		isDeclaration(LAYOUT, path) || path.startsWith('apps/edge/gateway/elsewhere/')
			? { packages: ['@monoflake/gateway'] }
			: {},
};

if (import.meta.main) main(CONFIG);
