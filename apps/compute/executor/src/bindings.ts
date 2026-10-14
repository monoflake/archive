/**
 * A workflow's `env`, as workerd would hand it service bindings: each name a `fetch` that sends
 * the path and query of whatever it is asked to the base it was declared with, the host ignored.
 * See spec/architecture/executor.md, "Bindings".
 */

/** What a service binding offers a workflow: `fetch`, and nothing of where it goes. */
export interface Fetcher {
	fetch(input: Request | string | URL, init?: RequestInit): Promise<Response>;
}

/** The address a binding's call goes to: the base, and the path and query of what was asked. */
export function addressOf(base: string, input: Request | string | URL): string {
	const asked = new URL(input instanceof Request ? input.url : input, 'http://binding.invalid');
	return `${base.replace(/\/$/, '')}${asked.pathname}${asked.search}`;
}

/** A binding over `base`, reached by `fetch`. */
export function binding(base: string, reach: typeof fetch = fetch): Fetcher {
	return {
		fetch(input, init) {
			const address = addressOf(base, input);
			return input instanceof Request
				? reach(new Request(address, input), init)
				: reach(address, init);
		},
	};
}

/** Every declared binding, by its name. */
export function envOf(bindings: Readonly<Record<string, string>>, reach: typeof fetch = fetch) {
	return Object.fromEntries(
		Object.entries(bindings).map(([name, base]) => [name, binding(base, reach)]),
	) as Record<string, Fetcher>;
}
