# Registry: what the platform holds of where

**`apps/data/registry` is the platform's record of where things are and with what**, kept in the
platform's Postgres, so the code's repositories hold only what an app needs -- RFC ja5hm. It holds
the nodes first; what each node runs and what each binding resolves to follow it there. Every
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

## Every relay follows it

**A relay asks the registry every 30 seconds and takes what it lists as its peers**, dialing a node
newly listed and stopping one no longer listed, without restarting -- [relay.md](relay.md). It asks
the cores the last answer named first, then every other node it holds, the first to answer being
the answer.

**What a relay last took is kept on its own disk, and it never expires.** A relay that reaches no
registry keeps the peers it has, and starts from them, with `RELAY_PEERS` under them as the seed for
a node that has never been told. The registry unreachable stops a change from spreading, never a
relay from running -- RFC ja5hm.

**An answer that lists no nodes, or not the relay's own, is not taken**: a registry emptied or
broken leaves the mesh as it was. Which answers are taken is `next_peers` in the relay's
`src/registry.rs`.
