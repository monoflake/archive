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
under a data key of its own, and the data key under the root key, both XChaCha20-Poly1305, both
bound to the secret's identity as associated data, so ciphertext moved to another row does not open.
A change is a row appended: who, when and which, never the value.

## Two classes of value, and a secret compared unread

**A value is of one of two classes, and `secret` unless its write declares `config`** -- RFC 231yq.
The service holds an app's whole environment, as a node splits it into `config.env` and
`secret.env`.

- **`config` is answered to anyone who may read the app's values**, whenever asked, as written:
  for now whoever may write its scope, and the node the app is placed on.
- **`secret` is answered to nobody**, but to the node it is placed on, which writes it into
  `secret.env`. A list says that it exists, its class and when it changed, never its value.
  The node's read carries each value's class; host writing a `config` to `config.env` instead is
  infra's to build.
- **A `secret` becomes `config` only by being written again with its value**, never by its class
  alone changed; a `config` may be made `secret` by its class alone.
- **A secret is compared without being read**: `POST` the value offered to its `compare` route, and
  the answer is `{"matches": true}` or `false`, the value held never replaced. The comparison is in
  constant time; it takes the right to write that secret; each secret answers five comparisons a
  minute across every core, counted in the history, and refuses the rest `429`; and each one is a
  row of the history -- who, when, which and the answer -- never the value offered. A `config` is
  read, not compared.

**Writing is granted by scope and app.** Besides `SECRETS_TOKEN`, which writes everywhere, a token
may be made for one app of one scope and writes only there, so a service that keeps values for its
resources -- the registry for an account's identities -- reaches its own and nothing else. A value
an app is given becomes its environment, so a scope token writing another app's would be code run
there: hence the app, and a write by a scope token is refused a name that changes how a process
starts -- `LD_*`, `DYLD_*`, `NODE_OPTIONS`, `PYTHON*`, `BASH_ENV`, `PATH` and their kind. **A node
is given values from the scopes `SECRETS_NODE_SCOPES` names alone**, `platform` when it names none,
so a scope that holds a resource's values -- the registry's `accounts` -- is never written into any
app's environment, whatever its apps are called: the node read once matched values by app name
alone, across every scope.

## The lock

**The root key is split two of three among the cores by Shamir's scheme**, each share on its core's
disk at `/data/seal/share`, outside every app's directory and every snapshot, mounted read-only at
`/seal` by host's `seal` role, which the cores alone grant -- infra's `spec/architecture/host.md`.
Each share carries a digest of the key it rebuilds, so a wrong share or a wrong key is refused
rather than used.

- **A copy starts sealed** and answers `503 service_unavailable` to every route that touches a
  secret, while `/health` answers, so a sealed deploy is not rolled back. Every five seconds it asks
  the other cores for their shares with `SECRETS_PEER_TOKEN`, and opens once two distinct shares
  agree. A core gives its share to that token alone, and only while it holds one.
- **A lone core is opened by hand**: `mise run seal unseal <node> <identity>` opens `recovery.age`
  with the offline recovery identity and sends the key, checked against the core's share, with the
  write token.
- **The lock is set once**, by `mise run seal init`. It makes the key on `tyo`, in the service's
  image with no network, and hands each core its share core to core. The only thing it returns is
  the key encrypted to `recovery.pub`'s post-quantum recipient, written as `recovery.age` to commit.
  It uses age's own program, pinned in the image, since the `age` crate cannot encrypt to an
  `age1pq1` recipient.

## The first run

**Once, in this order**, each step safe to run again:

1. `mise run database grant platform secrets` makes `platform_secrets` and writes `DATABASE_URL` to
   each core's `secret.env`.
2. `mise run seal token` makes `SECRETS_TOKEN` and `SECRETS_PEER_TOKEN` in `secrets.json` the first
   time, to be committed, and writes them to each core with every node's digest, and `CORES` to
   `config.env`.
3. `mise run seal prepare` makes each core's empty `/data/seal`, owned by the service, mode 700:
   host mounts it, and Docker refuses the container while it is missing.
4. The cores grant `secrets:peer` and `secrets:seal`, from infra's `CORE_GRANTS`, written to host's
   `GRANTS` by `mise run //repos/infra:node <core>`.
5. `mise run //repos/infra:node deploy <core> --run <run> --app secrets` on each core; each answers
   `/health` `{"sealed": true}` and waits.
6. `mise run seal init`, after its `--dry-run`, sets the lock, and `recovery.age` is committed.
7. Each core's `GET /v1/seal` answers `{"sealed": false, "share": true}` within seconds, with no
   restart; one core redeployed opens again from the other two.

## Who reads what

**Writing and listing take `SECRETS_TOKEN`; a node reads only the secrets of the apps the registry
places on it**, `GET /v1/nodes/{node}/secrets`, with its own token. That token is derived and never
stored: HMAC-SHA256 of `platform secrets read` under the node's `HOST_TOKEN`, which is already root
on that node. The service keeps only each one's SHA-256 digest, written by `mise run seal token`, so
its environment reads nothing. host is to write them into each app's `secret.env` and keep what it
last read, so the service down stops a change to a secret, never a node. Not built yet.
