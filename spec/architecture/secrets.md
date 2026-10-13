# Secrets: what the platform keeps that nobody may read

**`apps/data/secrets` holds every tier 1 secret, encrypted in the platform's Postgres under a root
key that lives only in its memory** -- RFC 231yq. Tier 0, what starts the platform, stays in
`secrets.json`, sops over age.

## Where it runs, and what it keeps

**It runs on the three cores, `tyo`, `sin` and `rdu`, its data in `platform_secrets`**, reached by
the `DATABASE_URL` that `mise run database grant platform secrets` writes. Every copy writes through
its own node's `primary`. It is deployed by hand, a core at a time, as the registry is: a copy
restarted finds the other two holding the key. Its port, 12014, is published to the tailnet alone.

**A secret is its scope, its app and its name, and its value is encrypted twice over.** The value is
under a data key of its own, and the data key under the root key, both XChaCha20-Poly1305, both bound
to the secret's identity as associated data, so ciphertext moved to another row does not open. A
change is a row appended: who, when and which, never the value.

## The lock

**The root key is split two of three among the cores by Shamir's scheme**, each share on its core's
disk at `/data/seal/share`, outside every app's directory and every snapshot, mounted read-only at
`/seal` by host's `seal` role, which the cores alone grant -- infra's `spec/architecture/host.md`.
Each share carries a digest of the key it rebuilds, so a wrong share or a wrong key is refused rather
than used.

- **A copy starts sealed** and answers `503 service_unavailable` to every route that touches a
  secret, while `/health` answers, so a sealed deploy is not rolled back. Every five seconds it asks
  the other cores for their shares with `SECRETS_PEER_TOKEN`, and opens once two distinct shares
  agree. A core gives its share to that token alone, and only while it holds one.
- **A lone core is opened by hand**: `mise run seal unseal <node> <identity>` opens `recovery.age`
  with the offline recovery identity and sends the key, checked against the core's share, with the
  write token.
- **The lock is set once**, by `mise run seal init`. It makes the key on `tyo`, in the service's
  image with no network, and hands each core its share core to core. The only thing it returns is the
  key encrypted to `recovery.pub`'s post-quantum recipient, written as `recovery.age` to commit. It
  uses age's own program, pinned in the image, since the `age` crate cannot encrypt to an `age1pq1`
  recipient.

## Who reads what

**Writing and listing take `SECRETS_TOKEN`; a node reads only the secrets of the apps the registry
places on it**, `GET /v1/nodes/{node}/secrets`, with its own token. That token is derived and never
stored: HMAC-SHA256 of `platform secrets read` under the node's `HOST_TOKEN`, which is already root
on that node. The service keeps only each one's SHA-256 digest, written by `mise run seal token`, so
its environment reads nothing. host is to write them into each app's `secret.env` and keep what it
last read, so the service down stops a change to a secret, never a node. Not built yet.
