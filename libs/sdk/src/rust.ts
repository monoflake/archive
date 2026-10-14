import { rustConstants } from '@canmi/me/rust';
import { URLS } from './index.ts';

/**
 * The constants host now gives each app at run time from infra's map, kept one release and marked
 * to go: a doc comment rather than `#[deprecated]`, since a comment changes no image and an
 * attribute rebuilds every one. See RFC zvrk7.
 */
const GIVEN_AT_RUN_TIME = new Set([
	'APPS_PRODUCTION_SITE',
	'APPS_PRODUCTION_API',
	'APPS_PRODUCTION_ALIAS',
	'APPS_PRODUCTION_SYMLINK',
	'APPS_PRODUCTION_CDN',
	'APPS_PRODUCTION_PANEL',
	'INTERNAL_APP',
	'INTERNAL_PANEL',
	'INTERNAL_KEEPER',
	'INTERNAL_HOST',
	'INTERNAL_LEDGER',
	'INTERNAL_SHOT',
	'INTERNAL_API_PRIVATE',
	'INTERNAL_API_PUBLIC',
	'EXTERNAL_ARTIFACTS',
]);

const DEPRECATED =
	"/// Deprecated: read the app's environment, which host writes from `[urls]` (RFC zvrk7).";

/**
 * The Rust mirror of the URL map.
 *
 * Rust cannot import this library, so every Rust program reads the generated `src/lib.rs`, the
 * `monoflake` crate, committed so a checkout compiles without Node having run. `mise run urls`
 * rewrites it and `rust.test.ts` fails when it no longer matches this render, so the two languages
 * cannot drift past `mise run verify`. See web's spec/architecture/workspace.md.
 */
export function rustUrlMap(): string {
	const constants = rustConstants(URLS).replace(
		/^pub const ([A-Z0-9_]+):/gm,
		(head, name: string) => (GIVEN_AT_RUN_TIME.has(name) ? `${DEPRECATED}\n${head}` : head),
	);
	return [
		'//! @generated from libs/sdk/src/index.ts by `mise run urls`; do not edit.',
		"//! One URL map for both languages -- see web's spec/architecture/workspace.md.",
		'',
		constants,
		'',
	].join('\n');
}
