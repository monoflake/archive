# Roadmap

Where the platform is going, without the steps. What is not done is an RFC -- the workspace's
`spec/planning.md`.

- **The platform deploys and operates itself before anybody else uses it**, measured by the acts
  still done by hand, [architecture/operations.md](architecture/operations.md) -- RFC bz8nz -- in
  this order: the nodes
  in its registry (RFC ja5hm and qcyb3); a controller in every layer bringing what runs toward what
  the registry wants (RFC jln34); every act done from the console through one contract (RFC dk6wz and
  cpc2b); what a node runs signed by the core (RFC 8vy7x); and only then accounts.
- **Control stands on Cloudflare's serverless primitives and schedules our nodes**, which build what
  serverless makes costly -- RFC o7eir, in five stages: control on Workers, D1 and Durable Objects;
  one model, the nodes the pool it schedules; one binding layer, a vendor's service or a node's
  behind one declaration, hosted projects in an account of their own; storage replaced beneath the
  app, encrypted, compressed and content-addressed; and a second vendor only once the platform is
  stable. The registry moves first, then secrets, then the ledger and the scheduler, then the
  deployer.
- **Every API is reached through one gateway**, whose declarations are the only description of a
  service's routes, limits and lifetimes -- [architecture/gateway.md](architecture/gateway.md). The
  move is done but for its cleanup.
- **A service runs on whichever placement suits it, and fails over to the next** --
  [architecture/services.md](architecture/services.md), "Cloudflare is the one entrance, and that is
  accepted".
- **The service domains each answer a page of their own**, with the platform's names laid over them
  where a host carries both.
- **An app declares what it needs and the platform places it**: N instances and their memory, keeping
  nothing; a scope's bucket, content-addressed and laid out across self-hosted and provider stores;
  SQLite or Postgres; shared within a scope and isolated between scopes -- [architecture/scheduling.md](architecture/scheduling.md). The per-app sidecars of
  [architecture/objects.md](architecture/objects.md) and
  [architecture/databases.md](architecture/databases.md) give way to it.
- **Three core nodes decide, and every node passes it on**: the control messages become one ordered,
  signed log that relays gossip and a returning node catches up on --
  [architecture/relay.md](architecture/relay.md).
- **One console shows every node and the deploy pipeline, served at the edge and live through the
  nearest node** -- web's `spec/architecture/console.md`, now that it is a services app there.
- **Apps are deployed through the platform, Workers carrying only their stateless part**: the
  platform deploys an app's Worker and hands it every binding, state coming from its own Postgres,
  buckets, scheduler and WebSocket services on the nodes -- the workspace's
  `spec/architecture/layers.md`, "Cloudflare is under the platform, and an app binds only the
  platform". The site is the first app to move, D1 to Postgres the largest step of it. The
  platform deploys a Worker as host deploys an image: CI uploads the bundle as an artifact and holds
  no credential, and a platform service on a core node takes it and deploys it with wrangler and a
  token of its own, scoped to Workers' scripts and routes -- Cloudflare's Scripts API, free, which
  covers code, static assets, bindings, custom domains and versions. Rejected: Workers for
  Platforms, made for running others' untrusted code in namespaces, which none of ours is, and paid.
