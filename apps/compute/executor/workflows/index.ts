/**
 * Every workflow this executor runs, by the name a run is opened with: its module and the class it
 * exports. Only the thread replaying a turn loads one. Test workflows alone, until `mirror`. See
 * spec/architecture/executor.md, "Where a workflow lives".
 */

import { EXTENSION, WORKFLOWS as DIRECTORY } from '../src/files.ts';

/** Where a workflow's code is: a module's URL and the name of its `WorkflowEntrypoint`. */
export interface Workflow {
	readonly module: string;
	readonly export: string;
}

/** `path` under workflows/, without its extension, which the sources and the bundle differ in. */
function at(path: string, name: string): Workflow {
	return { module: new URL(`${path}${EXTENSION}`, DIRECTORY).href, export: name };
}

export const WORKFLOWS: Readonly<Record<string, Workflow>> = {
	echo: at('test/echo', 'Echo'),
	nap: at('test/nap', 'Nap'),
};
