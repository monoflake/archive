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

## One task, three verbs

`mise run control <verb> [--dry-run]` in this repository, `--dry-run` touching nothing:

- **`deploy`** applies the D1 migrations in `apps/control/control/migrations`, then deploys the
  Worker. wrangler is given `CLOUDFLARE_CONTROL_TOKEN` and the one account that token reaches, found
  by asking Cloudflare, in its own environment alone; neither is printed.
- **`token`** draws `CONTROL_READ_TOKEN` and `CONTROL_IMPORT_TOKEN` anew and puts each on the Worker
  and into `secrets.json`, on stdin alone. It refuses while the Worker is not there, since a secret
  put on a missing Worker makes an empty one of that name. A route whose token is not set answers
  `503`.
- **`import`** copies the old registry into control whole, and is run again at will while the old
  registry still takes the writes.

## What it holds and answers

**A resource is its 16-hex id, kept from the old registry, and a node has no name**: what control
shows a node as is its `city`, `country` and `continent`, from infra's `nodes.toml`, and an app's
`placements` are node ids. An app keeps its name. A spec is stored as JSON with every object's keys
sorted, so two specs compare as text.

**It answers as the old registry does**, in `@canmi/response`'s envelope, so a caller changes only
its base URL and its token: `GET /health`; `GET /v1/{nodes,apps,accounts}`, an app list narrowed by
`?name=` or by `?node=<node id>`; `GET /v1/{plural}/{key}`, a key of an id's shape an id and any
other an app's name. Every route but `/health` takes the read token. Writes stay with the old
registry until the deployer and the console move.

**An import replaces every resource in one batch and continues the old registry's revisions**, so
a write made later is numbered above the newest change copied. It is refused with `409` when control
already holds a newer change than the one it brings.
