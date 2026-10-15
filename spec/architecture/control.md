# Control: the API that controls the platform

**`apps/control/control` is the platform's control, a Worker over one D1 database in a Cloudflare
account of its own** -- RFC o7eir. It is not the gateway: the gateway is the API the platform
serves, and control is the one that runs the platform itself. It takes the registry's place one
part at a time; until a part has moved, [registry.md](registry.md) still rules it.

## Where it runs

**The account is `monoflake`, on the free plan, apart from the one holding the gateway and every
other Worker**, so nothing a hosted project does reaches control or its quota. A feature is used
there only once it is confirmed free and failing closed -- an exceeded limit an error, never a
charge. The database is `control`, placed in western North America (`wnam`), and the Worker runs
with Smart Placement beside it.

**It answers at `control.monoflake.net`**, `URLS.control` in `libs/sdk`, a custom domain attached
in the dashboard; `wrangler.jsonc` declares no route and no `workers.dev` host. The console takes the
apex of `monoflake.net` behind Cloudflare Access; control is outside Access, since what calls it
proves itself by a bearer now and a node's key later.

**Neither CI nor the deployer deploys it**: it has no `service.toml`, so no list of the platform's
apps names it, and the deployer's token reaches only the other account.

## One task, four verbs

`mise run control <verb> [--dry-run]` in this repository, `--dry-run` touching nothing:

- **`deploy`** applies the D1 migrations in `apps/control/control/migrations`, then deploys the
  Worker. wrangler is given `CLOUDFLARE_CONTROL_TOKEN` and the one account that token reaches, found
  by asking Cloudflare, in its own environment alone; neither is printed.
- **`token`** draws `CONTROL_READ_TOKEN`, `CONTROL_WRITE_TOKEN` and `CONTROL_IMPORT_TOKEN` anew and
  puts each on the Worker and into `secrets.json`, the read and write ones into infra's too, on stdin
  alone. It refuses while the Worker is not there, since a secret put on a missing Worker makes an
  empty one of that name. A route whose token is not set answers `503`.
- **`writers`** gives the writers what they write with: the deployer's `CONTROL` and
  `CONTROL_WRITE_TOKEN`, with the GitHub token, its sources and the artifact bucket, on its node,
  and both tokens to the console Worker as secrets -- what `registry token` did before the cut.
- **`import`** copied the old registry into control whole. The cut ended it on 2026-10-15: the cores'
  registry stopped, and `CONTROL_IMPORT_TOKEN` was deleted from the Worker and `secrets.json`, so the
  route answers `503`.

## What it holds and answers

**A resource is its 16-hex id, kept from the old registry, and a node has no name**: what control
shows a node as is its `city`, `country` and `continent`, from infra's `nodes.toml`, and an app's
`placements` are node ids. An app keeps its name. A spec is stored as JSON with every object's keys
sorted, so two specs compare as text.

**It is the lower layer's gateway as well as control**, decided with the author on 2026-10-15: every
address is `/v{n}/{scope}/*`, the registry the scope `registry`, so a second scope is one more
mount, and one worth a Worker of its own is split out only then. `/v1/health` is the scope `health`, whether control and its database answer, the one route with no
bearer; a scope it does not hold answers `404
no_such_scope`, and a path no scope takes `404 no_such_route`, both in the envelope.

**The registry scope answers as the old registry does**, in `@canmi/response`'s envelope, so a caller changes only
its base URL and its token: `GET /v1/registry/{nodes,apps,accounts}`, an app list narrowed by
`?name=` or by `?node=<node id>`; `GET /v1/registry/{plural}/{key}`, a key of an id's shape an id and any
other an app's name. Every read takes the read token or the write token, since a writer reads
what it is about to change.

**It takes writes as the old registry did**, each with `Authorization: Bearer CONTROL_WRITE_TOKEN`
and a body `{by, revision, spec}` held to the revision read, answered `202 {revision, task}` or,
when nothing changed, `200 {}`; a write that lands adds a `changes` row naming `by`:

- `PUT /v1/registry/apps/{name}`: revision `0` creates the app, any other replaces its spec.
- `PATCH /v1/registry/{nodes,apps,accounts}/{key}`: an RFC 7386 merge of `spec` over the one held, `null`
  removing a key.
- `POST /v1/registry/{nodes,accounts}`: revision `0`, answered `{id, revision, task}`, the id drawn by
  control. A
  new node's id is then written into infra's `nodes.toml`, which `mise run node` reads it from.
- `GET /v1/registry/kinds`: the catalog, `catalog.json` and its schema beside the Worker, read by the console
  and the command line through the reader `mise run catalog` makes; a node is unnamed and
  `placements` are node ids.

Refusals are the old registry's: `409 invalid_revision`, `404 no_such_resource`, `400
invalid_body`, `forbidden_field`, `invalid_name` or `invalid_resource`, `401 invalid_token`, `405
invalid_method`, `503` while a token is not set. **An app's spec is checked as the old registry checked it**, `placements` aside: those are
node ids, each once, in the order given, and one an app gains must be a node control holds and has
not retired, so retiring a node never blocks a release of an app still placed there. A node has
`city`, `country` and `continent`, required; an account's identity is held by one account alone, a
table keeping it so.

## Who reads it, and with what

**A node is its 16-hex id in every contract; its three-letter name is only its hostname**, the
name an operator reaches it by -- RFC o7eir. Placements are control's alone: a `service.toml` names
no node, and says `worker = true` when the app is a Worker. Until the public gateway is taken up again,
the declarations keep the node lists the gateway's scope table, the hook's receivers and the change
engine read; host, the secret service and every operator task read control's alone.

| Reader                            | Asks                                                      | Every    | Is given                                                                    |
| --------------------------------- | --------------------------------------------------------- | -------- | --------------------------------------------------------------------------- |
| host                              | `GET /v1/registry/apps?node={NODE_ID}`                    | 60 s     | `NODE` (hostname), `NODE_ID`, `CONTROL`, `CONTROL_READ_TOKEN`               |
| the secret service                | `GET /v1/registry/apps?node={id}`, for the node asking it | 60 s     | `CONTROL`, `CONTROL_READ_TOKEN`; node tokens keyed by id                    |
| relay                             | `GET /v1/registry/nodes`, its mesh keyed by id            | 300 s    | `NODE_ID`, `CONTROL`, `CONTROL_READ_TOKEN`, `RELAY_PEERS` as `id=host:port` |
| the deployer, `node deploy --app` | `PATCH /v1/registry/apps/{name}` with a release           | per run  | `CONTROL`, `CONTROL_WRITE_TOKEN`                                            |
| the console                       | everything                                                | per page | `CONTROL_READ_TOKEN`, `CONTROL_WRITE_TOKEN` as Worker secrets               |

**Every app is told `NODE_ID` beside `NODE`**: what keys data by its node -- relay, the ledger, the
executor, the probe -- keys it by `NODE_ID`; `NODE` stays the hostname, which Patroni names its
members by. Together the readers ask about 29,000 times a day, inside the free plan's 100,000;
RFC o7eir's one speaker for the cluster brings it lower.

**An import, `PUT /v1/registry/import` with `CONTROL_IMPORT_TOKEN`, replaces every resource in one
batch and continues the old registry's revisions**, also rebuilding the identities table, so
a write made later is numbered above the newest change copied. It is refused with `409` when control
already holds a newer change than the one it brings.
