// A bearer per route for now; a node proves itself by its Ed25519 key later. RFC o7eir.

const encoder = new TextEncoder();

async function digest(text: string): Promise<Uint8Array> {
	return new Uint8Array(await crypto.subtle.digest('SHA-256', encoder.encode(text)));
}

/**
 * Whether a request carries the bearer a route takes, compared in constant time: both sides are
 * hashed first, so neither how much matched nor how long the token is shows in the timing.
 */
export async function admitted(authorization: string | null, token: string): Promise<boolean> {
	const given = authorization?.startsWith('Bearer ') ? authorization.slice('Bearer '.length) : '';
	const [left, right] = await Promise.all([digest(given), digest(token)]);
	const difference = left.reduce((held, byte, index) => held | (byte ^ (right[index] ?? 0)), 0);
	return difference === 0;
}
