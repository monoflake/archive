# Relay: three nodes decide, every node passes it on

Where the platform's control messages are going, and none of it is built. Today a finished run
reaches nodes as one notice per node, sent once by `hook` -- [services.md](services.md), "Every node
is the same node"; a node that was away then never hears of it.

## Three nodes agree, and the rest pass it on

**Three core nodes, one per failure domain, decide what happened: `tyo`, `sin` and `rdu`.** A
message is committed once two of the three have confirmed it, and committed messages form one log,
numbered in order and never rewritten. The nodes and their domains are infra's
`spec/architecture/nodes.md`, "Three nodes are the core, named by the author"; three in three
domains survive the loss of any one account.

**Every other node is a relay.** It keeps the log, and passes it on to whoever asks. A node does not
care where it heard a message: it compares how far its log goes with a neighbor's and takes what it
lacks. One number says whether a node is behind, because the log is in one order -- which is what
deciding among three first buys.

**A committed entry is signed by the core**, so a relay can carry it and cannot forge it. Any node
taken over can withhold messages, never invent one.

**A node that was away catches up by the same comparison**, from whichever neighbor it reaches
first, which is what makes a node allowed to go offline -- `home` -- whole again when it returns.

## What travels on it

A run that finished is the first message: `hook` hands it to the core instead of to every node. What
a run built can follow the same way: GitHub records each artifact's digest, so a node can take the
bytes from a neighbor and check them, and a node without IPv4 then needs nothing from GitHub.

## The live half is built first

**`apps/system/relay` runs on every node, as a peer** -- infra's `spec/architecture/host.md`, "A role
is asked for by the app and granted by the node" -- on port 12012, its port open to the other
nodes over the tailnet and host's own network open to it. Every three seconds it reads its node's
host with the read token: the events, the apps, the machine's readings. The apps and the
readings are the node's snapshot; the events go into the runs -- "The runs, mirrored on every
relay's disk" -- and are never carried in a snapshot, RFC grmtb. The node is the only authority on it: a neighbor's word on a node's own snapshot is
never taken.

**A snapshot's version is its origin's clock, in milliseconds**, or one past the last where the
clock has not moved, so a relay that restarts starts above whatever its neighbors still hold and
keeps nothing on disk. Comparing versions is the whole of "is this behind".

**A relay pushes its own snapshot when what it says changes, a new version each time, and a beat
every round; it passes the others on by comparison.** Every two seconds each tells its neighbors
which versions it holds and each node's newest beat, and each answers with what the other lacks.
A neighbor whose first message did not say it hears by beats is older than them: while one is met,
a relay gives its own snapshot a new version every round, as before, so that neighbor still hears
it -- RFC grmtb. Flooding every snapshot on arrival would carry each about seventy times
across a full mesh of seven every three seconds; comparing costs a hop up to two seconds instead. A
node that cannot reach a third directly still hears it through a neighbor.

**Between two relays whose first messages both say `sync`, the comparison is a sync and a change a
delta** -- RFC grmtb. Every three seconds each sends, per node it holds, its version, the digest of
it and its newest beat, so its own beat rides in every one. The digest is `xxh3-64`: 64 bits of
xxh3 over the snapshot written compact with its keys sorted, as sixteen hex digits. The other end
answers each node it holds newer with a delta from the version said, or whole where that version
is no longer among the last four it keeps, and each node at the same version with another digest
whole. A relay's own change goes to each neighbor as a delta from the version that connection
holds: the keys changed, set whole or named gone, and `apps` as rows by name. **A delta names the
version it applies to and carries the digest of its result**; a relay holding another version, or
reading another digest, asks for that node with `want` and is sent it whole. There is no forward
error correction: a frame arrives whole or the socket drops. With a neighbor that does not say
`sync`, the two-second comparison stands.

**The mesh is `/mesh`, a WebSocket each relay dials on every other**, admitted by a secret the
relays share, `RELAY_SECRET`. The peers are every node control lists and has not marked `retired`, by id, followed every five
minutes with `GET {CONTROL}/v1/registry/nodes` and kept on its disk -- [control.md](control.md), RFC o7eir --
with `RELAY_PEERS`, `id=address:port` by tailnet address, the seed a relay starts from; a node is
its `NODE_ID` everywhere in the mesh, and a relay speaking another contract version is refused; a relay skips
its own, and refuses a neighbor it holds as retired. Each stream's first message
carries the contract's version, and a neighbor speaking another is dropped. Both ends ping every
three seconds -- the round trip below. **Each socket is read by its conversation and written by a
task of its own**, from a queue of 64 frames, so a write waiting on a neighbor never stops the
reading that the neighbor, waiting on its own write, needs: two ends each waiting on a write the
other never read jammed until the drop, on `sin` and `nrt` on 2026-10-14 -- RFC grmtb. A frame
that finds the queue full is dropped, never waited for, and the next comparison sends what it
lacks. A socket on which nothing has been heard for ninety seconds is dropped, and a write that has
waited ninety seconds is given up: a neighbor gone without closing stops taking what is written,
and TCP alone would hold that write for a quarter of an hour, as it did on `nrt` and `hnd` on
2026-10-13. A dropped socket is redialed from one second, doubling to a minute. A
browser's WebSocket is pinged every thirty and dropped by the same ninety.

**A browser opens `/live` on `relay.canmi.app`**, behind Access, and is sent the whole cluster, then
every change; `/state` answers the same once, for the console's polling. **`/live` is an event
stream where the reader asks for one with `Accept: text/event-stream`, and a WebSocket where it
asks for an upgrade**, the two side by side until every reader has moved -- RFC grmtb. Each event
is a `data:` line carrying the message a socket is sent, the whole cluster first; `: ping` goes out
after fifteen seconds of nothing else. A stream that ends is opened again whole, never resumed,
since its reader may land on another relay. Caddy compresses it as it does any answer, flushed per
event, and decides on the first write, which the cluster always clears at 512 bytes; its gzip
does not carry matches across flushes, so a round's beats go as one event rather than one a node:
nine small events each paid gzip's per-flush cost, about 7.3 KB a minute for one page, and one event
a round is about 1.5 KB, measured through Caddy on 2026-10-14. A page is sent `beats` on the event
stream and the WebSocket alike; the mesh keeps its own beat in the sync. **`/live` admits a page on
`.app` alone**, by its `Origin`: Access lets a reader in by a cookie the browser sends on any page's
WebSocket, so without the check any site the reader visits could open it as them. A request with no
`Origin` comes from no browser and carries no reader's cookie, and passes; the private mirror's
pages are refused, since the console is served on `.app`. Each node's entry carries `heard_at`, its
origin's newest beat or version held, never later than this relay's clock, `state`, and `runs`, the
version of the node's runs this relay holds, absent where it holds none; a peer never heard is an
entry carrying `state` alone, and `runs` where it is held. A state that time alone changes is sent
again on `/live` at the same version, so a reader keeps dropping an older version but takes the
same one with another `state`. **A node's runs moving sends its entry again at the same version**,
with another `runs`, so a page reads `/runs?since=` again rather than being sent the rows -- RFC
grmtb. An app in a snapshot carries `driver: true` where host runs it as a driver, which has no
container of its own and so is never `running`; the key is absent for every other app, whose bytes
stay as they were. An entry also carries `readings`, its newest minute's mean and highest of each
of the meter's metrics, and `round_trip`, this relay's own as timed now and any other node's over
its newest minute; between entries a browser is sent `beats`, once a round, every node heard since
the last as its new `heard_at`, `{"type":"beats","beats":{node: heard_at}}`; nothing goes in a
round that heard none, and, for a node a page on this relay watches, `readings`, its second as its origin reads it. A socket opened with
`deltas=true` is sent, after the whole cluster, each node's changes rather than its entry whole:
`delta`, its keys set or gone and its snapshot's as the mesh's delta says them, with `base`, the
version of the entry it was last sent. A browser holding another version opens the socket again.

## The console asks, and the relay answers from memory

**Every read the console's server makes of a relay is answered at once, from what the relay already
holds.** The console's server is a stateless front end that draws and never waits -- web's
`spec/architecture/console.md`, "The console's server never waits on data; it only draws" -- and the
relay of the node nearest it is its backend: `/state` is the whole cluster as held, with no read of
host or of a neighbor on the way. A read the console needs that the relay cannot yet answer that way
is held by the relay before the console reads it, never gathered while the console waits; what is
still gathered in the console is listed in RFC mij2d. Decided with the author on 2026-10-09.

## A node says it is leaving before it goes

**On `SIGTERM` a relay takes a last snapshot of its own carrying `leaving`, and its neighbors are
told before it stops**, so a node going down on purpose is shown as leaving rather than lost. One
hook covers a deploy of the relay, a restart, a `docker stop` and a reboot, since dockerd stops
every container with the same signal. A crash says nothing, and its node goes late, then gone, as
before.

- **`"leaving": { "reason": "upgrade" | "restart", "within": 180 }`**, in seconds. The reason is
  read from the relay's host once, within a second: `upgrade` while host's newest row for the relay
  is a deploy, a rollback or a rollback with its data still running, `restart` otherwise, a reboot
  and a stop by hand included, which the relay cannot tell apart. 180 seconds covers a cloud
  machine's reboot, `/data` mounted and the health check passed.
- **The relay waits up to two seconds for the snapshot to be written to each neighbor's socket, then
  stops**, inside the twenty seconds every container is given -- infra's
  `spec/architecture/host.md`, "The control plane going down is not an outage". The snapshot is
  versioned and gossiped like any other, so a neighbor that missed the push takes it by comparison,
  after its origin has gone.
- **Each node's entry carries `state`, the relay's own reading of it**, which the console draws and
  never computes: `live` heard within 10 seconds, `late` within 60, `upgrading` or `restarting`
  while an announced `leaving` is within its `within` of `heard_at`, `waiting` for a peer not yet
  heard in the relay's first 60 seconds, and `gone` otherwise. A newer version without `leaving` is
  the node back. A state that time alone changes is sent again on `/live`, by a sweep every second.
- **A node's own snapshot is a new version only when what it says changes** -- its apps, the
  meter's `info`, `stale`, `leaving` and `alarms` -- **and a node is heard by its beat**: a few dozen
  bytes to every neighbor every round, saying only that it is alive -- RFC grmtb.
- **`heard_at` is its origin's newest beat held, or its version where that is newer, never later
  than this relay's clock**: both are the origin's clock in milliseconds, so a relay that starts and is handed a neighbor's snapshot
  five minutes old holds it as five minutes old, not as heard now. Clocks on the tailnet agree
  within a second, against thresholds of ten and sixty.
- **The relay reaches the canary first**, as host, keeper and Caddy do -- infra's
  `spec/architecture/host.md`, "A new host, keeper or Caddy reaches the canary first" -- so a run
  that rebuilt it restarts nrt's alone, and the rest announce their leaving to a relay already back.

Decided with the author on 2026-10-09.

## A node's readings stay on the node

Its meter keeps them -- infra's `spec/architecture/meter.md`, "Retention" -- and the mesh carries
its minutes alone. A page open on a node holds a lease on its second, `POST /watch?node=`, sixty
seconds long and renewed every thirty while the page is shown, asked of the relay its stream is on.
That relay asks the node's own over the mesh, which sends its second every round to it alone while
the lease holds. A node's charts read its own host through its binding, and where it does not
answer, the nearest relay's minutes of it. **A threshold is judged on the node, from each minute**:
the table in `alarm.rs` -- processor at 90% or more for five minutes, memory at 90% for five,
storage at 90% at once, any temperature zone at 85 C for five -- each said in the snapshot as
`alarms`, a new version, since the first minute it held, and cleared by the first minute below its
clear line, 85% for processor and memory, 88% for storage, 80 C for a temperature, so a reading at
the line does not set and clear it every minute. Decided with the author on 2026-10-14, RFC grmtb.

## The runs, mirrored on every relay's disk

**Every relay keeps a mirror of every node's deploy rows of the last 30 days, in a SQLite file of
its own**, so the console's runs -- the overview's deploys, what happened, what failed, Deployments
-- are one read of the nearest relay, answered from its disk. A row is host's event row as
`host.rs`'s `Event` carries it, tagged with the node it happened on. Rows have one writer each: a
node's rows are its host's, read by its own relay and by no other, so the mirror needs no leader and
no agreement -- each origin's rows are compared by version and the newer kept, as a snapshot is. The
mirror is never the record: host's `history.db` is, keeps every row and is what a backup takes. A
mirror lost or corrupted is rebuilt from the relay's own host and from its neighbors, so it is never
backed up. Decided with the author on 2026-10-09.

- **A row's version is its origin relay's clock, in milliseconds**, given when that relay first
  reads the row or reads it changed -- a deploy moves through its stages -- and one past the
  origin's last where the clock has not moved, as a snapshot's version is.
- **What changed in the last three minutes is held in memory, the window**, and spread on the mesh
  as snapshots are: pushed by its origin, compared and passed on by the rest. A neighbor behind by
  more than the window is answered from the disk, by origin and above the version it holds.
- **The window is written to disk every 30 seconds, six chances a row inside it**: a write that
  succeeds marks what it took as written and the next chances leave it be; a row leaves memory once
  it is written and older than the window, never before, so a disk that refuses five writes in a row
  loses nothing, and a refusal is logged with the part, as a host read's is.
- **Rows older than 30 days are dropped, and at most 5,000 a node are kept**, the oldest going
  first; the drop runs with a write.
- **On start a relay reads its file, then its host's rows back to 30 days** by host's own
  paging, `/api/events?limit=50&before=<id>`, stopping at a row past 30 days, a short page or 5,000
  rows, and asks its neighbors for every origin above the version its file holds; the live half
  waits on neither, since no neighbor is ever asked for a relay's own rows. A page host does not
  answer is asked again 30 seconds later.
- **A file that cannot be opened is moved aside, never mended**: renamed with the time beside it,
  logged, and a fresh one opened, which host and the neighbors refill; the relay stops only where
  even that fails.
- **`/runs` answers the mirror and the window together**, every node's rows of the last 30 days,
  newest first, each with its node; the console groups them into runs for display -- web's
  `spec/architecture/console.md`, "The console's server never waits on data; it only draws".
- **`/runs` takes what a reader of a span or a count needs, and filters only**: `since`, a moment
  in milliseconds, answers the rows that started then or later and every row still running however
  long ago it began; `lean=true` leaves out the image, the stage, the commit, and why a row failed
  where it did not. The console counts and groups as before, from less: a week's steps, a day's
  verdict, the month's figures lean. A relay before these ignores them and answers every row whole,
  which the console, filtering by its own moment, reads the same. Decided with the author on
  2026-10-10, over counting in the relay, which would have kept the console's grouping twice.
- **The console's live views read the runs, not the snapshot**: the overview's verdict and
  timeline ask the last day's steps again each time a node's `runs` moves on `/live`, and
  Deployments reads again while one has moved in the last ten seconds.
- **The file is `/data/runs.db`**, `[data]` in `service.toml`, so the relay is rolled out as an app
  with state on its node is -- infra's `spec/architecture/host.md`, "An app chooses how it is rolled
  out, and keeping nothing earns a gapless one".

## Each node's minutes, kept for a year

**Every relay keeps every node's history a minute at a time, as it keeps its runs**, so the console
can draw a node over an hour or a year from one read: whether its apps ran, whether it was heard and
how far its neighbors were, and what it deployed. A node's minutes are its own relay's, written by
nobody else, versioned and spread on the mesh as its runs are -- "The runs, mirrored on every
relay's disk" -- into the same file, and answered from it. Decided with the author on 2026-10-10.

- **A minute is one row of its node's**: the minute it is, as its start in the origin's clock;
  `beats`, the rounds that minute that read host, of the twenty it holds; `down`, each app that
  should run and did not -- never a driver, which runs no container -- with how many of the minute's twenty rounds it was down -- an hour or a
  day sums those rounds, and the console decides how short a dip counts; `held`, how many were
  stopped on purpose; `round_trip`, each neighbor's mean round trip over it, in seconds, a neighbor
  not timed absent; `readings`, the meter's mean and highest of each metric over the minute --
  `cpu.usage`, `load.1`, `memory.used`, `swap.used`, `storage.used`, `network`, `temperature` -- read
  from host's `/api/node/series?grain=minute` as the minute ends, and folded into hours and days as
  their mean over the minutes and the highest; and `leaving`, the reason where the relay said it
  was leaving in it. The origin
  writes the row at the first round of the next minute. **A relay that says it is leaving ends the
  minute there and writes it at once**; one back inside the same minute joins it, keeping the word,
  so an upgrade's leaving minute is not written over.
- **A minute missing is the node unheard**: the origin writes nothing while it is down, so a gap in
  its minutes is that time, announced where the minute before it carries `leaving` and unannounced
  where it does not. A minute is due one minute after it ends, counted from the first minute held
  of the node.
- **Kept in three tiers, each folded from the one under it**: minutes for two days, hours for 30
  days and days for 400, an hour or a day holding the minutes it held, the sum of its beats, each
  app's rounds down, the minutes leaving, `announced` -- the minutes missing after one that said
  leaving -- whether it ends on such a minute, so a gap running across folds stays announced, and
  each neighbor's round trip as its mean and its worst. Missing is not stored: it is the due less
  the held, worked out as the answer is, since what is due starts at the node's first minute.
  **Each tier is dropped one whole unit of the one above at a time**: a minute goes with its hour
  once that hour ended two days ago, and an hour with its day once that day ended 30 days ago, so
  a unit is held in exactly one tier and a slot sums what it holds. **Only minutes travel on the
  mesh**; hours, days and run days are each relay's own folds, the same wherever they are made
  from the same minutes, so a fresh relay starts with the two days of minutes its neighbors hold.
- **The runs are folded to days as well**, a count a node and an outcome, kept 400 days beside the
  mirror's 30, each day folded just before its rows are dropped. A run is a CI run on a node, each
  app at its latest row, or an action no run started grouped by its source, app and start minute;
  running where any row is, partly failed where some succeeded and some failed, a skipped row not
  counted. A day's count names the runs that went on past it, so the next does not count them
  again. Rows dropped by the cap of 5,000 a node never reach a day's count.
- **`GET /history?span=<seconds>&slot=<seconds>`** answers every node over the `span` ending now,
  cut into slots of `slot` seconds that fall on multiples of their length in UTC, the last holding
  now, each slot read from the finest tier that still holds it: `runs` by outcome, `succeeded`,
  `failed`, `running` and `partly_failed`, a slot of whole days reading a folded day's count and a
  finer one the mirror; `beats` and `due`; `down`, each app's rounds; `leaving`, `missing` and
  `announced`; `round_trip` to each neighbor, mean and worst; and `readings` where asked with
  `readings=true`, with `node` to answer one node alone. A slot must be whole minutes,
  whole hours past two days and whole days past 30, at most 1,000 slots and 400 days; anything
  else is a 400 `invalid_series`. Absent, not null, where a slot holds nothing -- the workspace's
  `spec/json.md`. The console asks it for a span longer than a day, the mirror's runs drawing the
  rest.
- **`/state`, `/runs` and `/history` are compressed**, gzip or brotli as the reader accepts, since
  `/runs` is hundreds of kilobytes, more than a far node sends plain inside the console's wait.
- **The mesh is compressed too, a zstd stream per connection each way**: level 3, a 128 KiB window,
  and each frame flushed as it is written, so it decodes the moment it arrives against everything
  its stream carried before. A relay sends compressed frames only to a neighbor whose first message
  says `zstd`, and the first message itself always goes plain. A frame that does not decompress
  drops the socket, and the redial starts both streams fresh. A connection's two streams hold under
  2 MB, about 1.2 for the one written and 0.6 for the one read. On recorded rounds the mesh
  compresses fourteen-fold, the comparison twenty-fold -- RFC grmtb. A browser's socket is not
  compressed here; see `/live`.

## The round trip to each neighbor

**A relay reads each neighbor's round trip from the kernel, and its minutes carry each neighbor's
mean per minute as `round_trip`**: `{ "tyo": 0.1512, "buf": 0.0184 }`, by node, in seconds to a
tenth of a millisecond. `/live` and `/state` show a node's from its newest minute and the relay's
own as timed now, so a page reads each node's round trip to every other from that node's own
entry. Asked for on 2026-10-09, for the
console's row of each node's round trip to the database's primary.

- **The figure is Linux's least round trip of the socket the relay dialed**, `TCP_INFO`'s
  `tcpi_min_rtt` -- the smoothed `tcpi_rtt` on a kernel before 4.6 -- read at every ping: it is
  timed on the connection's ACKs beneath the relay's own queues, and as the least it leaves out the
  delayed ACKs a mostly one-way socket draws. Pings timed alone counted the backlog, and Nagle's delay
  besides: on 2026-10-13 `sin` read `tyo` at 137 ms and `rdu` at 1,033 ms, where `tailscale ping`
  gave 69 and 276; the smoothed figure still read `nrt` from `hnd` at 19 ms, where it is one. Every
  mesh socket, dialed and accepted, sets `TCP_NODELAY`. The least is kept over the kernel's window,
  five minutes by default, so a path grown slower shows within it. Where the kernel's figure cannot
  be read, off Linux or on the accepting end, the timed ping below stands, and a kernel figure read
  lately is preferred to it.
- **The WebSocket's own ping is timed, not a message of the mesh's.** The other end's socket
  answers it beneath the relay with a pong carrying the ping's payload back, and the payload is
  when the ping was sent, so nothing is kept per ping and a late pong still times its own. A
  relay of an older build answers it the same, and the mesh's messages and contract are untouched.
  It measures the relays' own path over the tailnet with a busy peer's queue included, which is why
  it is only the fallback; it also keeps a socket known to be alive.
- **Every three seconds, on every socket, the first as the socket opens** -- the same `own::EVERY`
  the snapshot is taken at, since a faster ping times a figure no snapshot carries and a slower one
  leaves the figure a round behind. A ping and its pong are two frames of a few bytes,
  control frames, never compressed. Both sockets between two relays time it,
  and the latest of either stands.
- **A neighbor not timed within three pings is absent**, never `null` or a last figure held on --
  the workspace's `spec/json.md`, "Absent, not null" -- and with none timed the key itself is
  absent, as `stale` is. A relay never times itself, so a node's own name is never a key.
- **Seconds, as the workspace's `spec/json.md` has every duration**, and the page says them in
  milliseconds: what is written for a machine follows the one rule, and what is shown to a reader
  is the page's own wrapping of it, as a node's code is wrapped in its city. It was `round_trip_ms`
  for its first hours on 2026-10-09 and was renamed before anything but the console read it. A
  round trip is never part of a version.
- **No contract bump**: an added optional key is not a schema change -- `spec/json.md`, "An optional
  key is not a schema change" -- and a relay carries a snapshot as the JSON its node wrote.

## The order it is built in

1. **A port on the tailnet**: host's `peer` role, which the live half above runs in.
2. **The log, with one writer.** `hook` writes to one core node, and every node gossips the log and
   catches up by it. A node that was away no longer misses a run.
3. **Three writers.** The one writer becomes the three, an entry committed by two of them.

Which consensus and which gossip is open -- RFC oq3rw.
