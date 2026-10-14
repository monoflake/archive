/**
 * Where the executor's other files are, as these sources lay them out: a turn's thread and what
 * it runs, the stand-in for `cloudflare:*`, and the workflows. The bundle puts its own layout in
 * place of this file; see scripts/bundle.ts.
 */

/** The thread's entry: the resolve hook, then the turn. */
export const THREAD = new URL('./thread.ts', import.meta.url);
/** What the thread runs once the hook is in place. */
export const TURN = new URL('./turn.ts', import.meta.url);
/** What `cloudflare:workers` and `cloudflare:workflows` resolve to. */
export const SHIM = import.meta.resolve('@monoflake/replay/cloudflare');
/** Where the workflows are, and the extension of their modules. */
export const WORKFLOWS = new URL('../workflows/', import.meta.url);
export const EXTENSION = '.ts';
