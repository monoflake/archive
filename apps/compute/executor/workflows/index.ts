/**
 * Every workflow this executor runs, by the name a run is opened with: its module and the class it
 * exports. Only the thread replaying a turn loads one. Test workflows alone, until `mirror`. See
 * spec/architecture/executor.md, "Where a workflow lives".
 */

/** Where a workflow's code is: a module's URL and the name of its `WorkflowEntrypoint`. */
export interface Workflow {
	readonly module: string;
	readonly export: string;
}

function at(path: string, name: string): Workflow {
	return { module: new URL(path, import.meta.url).href, export: name };
}

export const WORKFLOWS: Readonly<Record<string, Workflow>> = {
	echo: at('./test/echo.ts', 'Echo'),
	nap: at('./test/nap.ts', 'Nap'),
};
