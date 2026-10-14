/** A failure the task reports as one line and exits 1 on. */
export class Failure extends Error {}

export function fail(message: string): never {
	throw new Failure(message);
}
