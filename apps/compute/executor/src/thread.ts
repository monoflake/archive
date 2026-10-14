/**
 * One turn in a thread of its own, the stand-in for workerd's isolate per call: a fresh global
 * scope, a heap of its own, and stopped by `terminate()`. This entry only puts the resolve hook in
 * place, then loads the turn, so nothing that names `cloudflare:*` is loaded before it. See
 * spec/architecture/executor.md, "A thread stands in for an isolate".
 */

import { registerHooks } from 'node:module';
import { SHIM, TURN } from './files.ts';

// What workerd provides by these names, the runner's own module stands in for.
registerHooks({
	resolve: (specifier, context, next) =>
		/^cloudflare:(workers|workflows)$/.test(specifier)
			? { url: SHIM, shortCircuit: true }
			: next(specifier, context),
});

void import(TURN.href);
