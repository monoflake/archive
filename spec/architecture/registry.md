# Registry: what the platform holds of where

**`apps/data/registry` is the platform's record of where things are and with what**, kept in the
platform's Postgres, so the code's repositories hold only what an app needs -- RFC ja5hm. It holds
the nodes and where each app runs; what each binding resolves to follows them there. Every
component that needs it reads it while it runs, and none restarts for a change -- RFC qcyb3.

## Where it runs, and what it keeps

**It runs on the three cores, `tyo`, `sin` and `rdu`, its data in `platform_registry`** in the
platform's cluster, reached by the `DATABASE_URL` that `mise run database grant platform registry`
writes -- [databases.md](databases.md), "The platform's alone, and its scope a role in it". Every
copy writes through its own node's `primary`, so any of the three answers alike, and a deploy of one
leaves the other two answering. It is deployed by hand, as `ledger` is, since a run cannot carry the
URL. Its port, 12013, is published to the tailnet alone, a peer every relay reaches.

**A node is a record, and a change to it is a row appended, never one overwritten** -- who made it,
when, what was and what is. A write that changes nothing writes nothing, so writing the same record
again is safe. The record is the node's table from infra's `nodes.toml` with its tailnet IPv4 added:
the axes infra's `spec/architecture/nodes.md` declares are checked, every other key kept as given,
so the node task can come to read it rather than the file.

**Reading is open to the tailnet and writing takes a token**, `REGISTRY_TOKEN`, made once in the
repository's secrets and written to the registry's nodes by `mise run registry token`; without one a
registry answers reads and refuses writes. `mise run registry import` writes every node of
`nodes.toml` with its address, the one way in until the console writes it -- RFC dk6wz.

## Nodes and apps, as resources of a kind

**`/v1` answers the control contract, RFC cpc2b**: a resource is `{kind, name, revision, spec}`
inside the `response` envelope, and a list is `{"items": [...], "revision": n}`, `revision` being
the newest change the registry holds. `GET /v1/kinds` is the catalog the console draws from: each
kind's fields, which can be edited, and the columns a list shows first. A kind it does not hold is
`404 no_such_kind`.

- **`node`** -- `/v1/nodes` and `/v1/nodes/{name}`, the record above as its `spec`.
- **`app`** -- `/v1/apps` and `/v1/apps/{name}`, its `spec` being `placements`, the nodes it runs
  on, each named once. `?node=<name>` answers only the apps placed on that node, which is how host
  asks what its node should run -- infra's `spec/architecture/host.md`. `mise run registry import`
  writes each app's placements from its `service.toml`, in every repository host deploys from --
  infra's `DEPLOY_SOURCES`, each found by `repos.toml`, read from each one's `main` and never its
  working tree, and a source not cloned stops it; a Worker's,
  placed on `workers`, is not the registry's. The import sets each app back to what git declares, so once the console
  writes placements it stops importing apps -- RFC dk6wz.

**A write carries the revision it read**, `{"by", "revision", "spec"}` with `revision` 0 for what
does not exist yet, and one made after another change is refused `409 invalid_revision`. A body
carrying `status` or `drift`, or a key a kind's `spec` does not hold, is refused `forbidden_field`.
A write answers `{"revision": n}`, or `{}` when it changed nothing; it becomes a task in the ledger
answering `202`, and `by` is taken from the request rather than the body, in a later wave -- RFC
cpc2b. A resource's revision is the change that last wrote it, so every kind shares one sequence.

**The routes before `/v1` stay until no relay reads them**, and they answer nodes alone: `GET
/nodes`'s `change` and `GET /changes` pass over every other kind's rows, so placing an app is not a
change a relay takes.

## Every relay follows it

**A relay asks the registry for `GET /v1/nodes` every 30 seconds and changes its peers by what the
answer says**, dialing a node newly listed and stopping one listed `retired`, without restarting --
[relay.md](relay.md). It asks the core nodes it holds first, then every other peer it holds, the
first to answer being the answer.

**A node leaves by being marked `retired`, never by going missing from an answer** -- RFC qcyb3.
`retired` is a boolean in a node's spec that an operator writes, absent meaning false. A node an
answer does not list stays a peer, so a write that drops a record by mistake disconnects nothing;
one listed `retired` is dropped, its relay is refused when it dials in, and a socket already open to
it is dropped within a comparison.

**What a relay has taken is kept on its own disk, and it never expires**: `registry.json`, every
node the registry has listed as it was last listed, and the revision taken. A relay that reaches no
registry keeps the peers it has, and starts from them, with `RELAY_PEERS` under them as the seed for
a node that has never been told. The registry unreachable stops a change from spreading, never a
relay from running -- RFC ja5hm.

**An answer that does not list the relay's own node is not taken**: a registry emptied or broken
leaves the mesh as it was. Which answers are taken, and what each one changes, is `next_peers` in
the relay's `src/registry.rs`.
