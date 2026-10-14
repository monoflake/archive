/**
 * The runner: one turn of a workflow written in Workflows' API, replayed against the ledger. The
 * executor's driver hands it a turn; see RFC 23wgz, "The second slice, the executor".
 */

export { CALLBACK_LIMIT_MS, milliseconds, wire, type WireConfig } from './config.ts';
export type { LedgerFetch } from './ledger.ts';
export { replay, type Ended, type Entrypoint, type Replay, type Turn } from './replay.ts';
