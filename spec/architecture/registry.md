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

**An app is a record too: where it runs, and what it runs there.** Its spec is `placements`, the
nodes it runs on, which a person sets, and `release`, the run of a repository that built what runs
and the commit it built (`{"repository": "owner/repo", "run": n, "commit": "<40 hex>"}`), which the
deployer on `tyo` writes after each run it takes -- RFC jln34. Absent, no release is recorded yet.
`PUT /v1/apps/<name>` sets the whole spec; `PATCH` sets only the keys it is given and keeps the
others, held to the revision read as `PUT` is, so a release written and an app moved never erase
each other. A node takes the same `PATCH`, so an operator's `retired` and an import never erase each
other either. `mise run registry import` writes placements by `PATCH` and creates an app it does not
find by `PUT`, so running it again keeps every release.

**Reading is open to the tailnet and writing takes a token**, `REGISTRY_TOKEN`, made once in the
repository's secrets and written to the registry's nodes by `mise run registry token`; without one a
registry answers reads and refuses writes. `mise run registry token` gives the token to the registry's writers too -- the deployer, wherever
its `service.toml` places it -- with `REGISTRY`, the cores' tailnet IPv4s, in its `config.env`, and
every source host deploys from added to its `DEPLOY_SOURCES`, so every app host runs gets a release. It
writes the platform's own `GITHUB_ACTIONS_TOKEN` beside them, the machine user's, and never an
organization's own token -- RFC 024i3. `mise run registry import` writes every node of `nodes.toml` with its address, the one way in
until the console writes it -- RFC dk6wz: by `PATCH /v1/nodes/<name>` at the revision it reads, and
by `PUT` for a node the registry does not hold, so it keeps every key `nodes.toml` does not carry,
`retired` among them.

## Nodes and apps, as resources of a kind

**`/v1` answers the control contract, RFC cpc2b**: a resource is `{kind, id, name, revision, spec}`
inside the `response` envelope, and a list is `{"items": [...], "revision": n}`, `revision` being
the newest change the registry holds. `GET /v1/kinds` is the catalog the console draws from: each
kind's fields, which can be edited, and the columns a list shows first. A kind it does not hold is
`404 no_such_kind`.

**Every resource has an `id`, and its name is a label** -- RFC 2kuvi. The id is 64 random bits as
16 lowercase hexadecimal digits, made when the resource is first written and never changed, unique
in its kind, so one that collides is made again; a migration gave every resource already held one.
A route's last segment is read as an id when it is 16 lowercase hexadecimal digits and as a name
otherwise, so a name of that shape can never be written -- a route reads it as an id and answers
`404` for one it does not hold, and the import refuses it as a name; a list
takes `?name=` to find one by name. Every row of the history carries the id of the resource it
changed. What refers to a resource -- a placement, a release -- still holds its name, until those
references move to ids in a contract of their own.

- **`node`** -- `/v1/nodes` and `/v1/nodes/{id}`, the record above as its `spec`, set whole by
  `PUT` and in part by `PATCH`. Beside the axes `nodes.toml` declares it holds the fields RFC 32hd9
  gives a node: `hosting`, `datacenter` or `selfhost`; `account`, its failure domain, written as
  `domain` is; and `roles`, one or more of `compute`, `storage` and `core`, each once. While
  `nodes.toml` still declares `tier`, `domain` and `core`, the import writes both, the new from the
  old -- `home` is `selfhost`, `domain` is `account`, and every node `compute`, a core `core` too --
  and the relay reads `roles`, falling back to `core`. The old keys go once `nodes.toml` writes the
  new.
- **`app`** -- `/v1/apps` and `/v1/apps/{id}`, its `spec` being `placements`, the nodes it runs
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
**A write that changes something is a task**, answering `202` with `{"revision": n, "task":
"<id>"}`, and one that changes nothing answers `200` with `{}` and makes none -- RFC cpc2b. The task
is the ledger's, service `registry`, kind `write`, its id 16 hexadecimal digits as a resource's is,
its summary the resource's kind, id and name and the revision written; it is recorded `done` with one
event, `written`, since the write is whole when it is answered, and the history row keeps its id.
The ledger is told as every service tells it and never waited on -- [ledger.md](ledger.md), "Pushed
to, never asking": the history is the record of what changed, and the task what the console draws.
`by` is still the body's, until the identity that writes is taken from the request, with accounts. A resource's revision is the change that last wrote it, so every kind shares one sequence.

**The routes before `/v1` are gone**: `GET /nodes`, `PUT` and `DELETE /nodes/{name}` and `GET
/changes` were removed once every relay followed `/v1/nodes`, and each answers `404 no_such_route`.
The `changes` table is kept whole as the history, the rows of a node the old `DELETE` removed
included; no route reads it, and a resource that does not exist is at revision 0 whatever of it the
history holds.

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
