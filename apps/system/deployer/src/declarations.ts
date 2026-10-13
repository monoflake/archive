/**
 * A run's `declarations` artifact, read for what decides a release: which of the images it built
 * is declared `rollout = "manual"`, rolled out by hand and so never given a release by a run --
 * RFC jln34. Read in memory, each entry held to the one shape the Deploy workflow writes.
 */
import { contentOf, entriesOf } from './artifact.ts';

/** `<app>.toml`, the one shape an entry of the declarations takes. */
const ENTRY = /^([a-z0-9](?:[a-z0-9-]*[a-z0-9])?)\.toml$/;

/** Why the declarations may not hold `name`, or nothing. */
function refusalOf(name: string, directory: boolean): string | undefined {
	return !directory && ENTRY.test(name) ? undefined : `${JSON.stringify(name)} is not <app>.toml`;
}

/**
 * Whether a service.toml declares its app rolled out by hand: `rollout = "manual"` among its
 * top-level keys, before any table. Read by pattern, as the hook reads `placements`, so the
 * deployer needs no TOML parser of its own.
 */
export function isManual(declaration: string): boolean {
	const top = declaration.split(/^\s*\[/m)[0] ?? '';
	return /^\s*rollout\s*=\s*"manual"\s*(?:#.*)?$/m.test(top);
}

/** Every app the declarations zip in `bytes` declares rolled out by hand. */
export function manualIn(bytes: Buffer): string[] {
	return entriesOf(bytes, refusalOf).flatMap((entry) => {
		const app = ENTRY.exec(entry.name)?.[1];
		return app !== undefined && isManual(contentOf(bytes, entry).toString('utf8')) ? [app] : [];
	});
}
