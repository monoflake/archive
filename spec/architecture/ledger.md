# `ledger`: every task any service was asked to do

`apps/data/ledger` is the one record of the work the platform's services take on: a capture `shot`
queued, and later whatever a scheduled job, a conversion or an import runs. A service keeps what it
needs to do the work; the ledger keeps that it was asked, by whom, how it went, and keeps it for
good. It is written to by every service and read by the console, so a task whose result is long gone
-- a picture rolled out of `shot`'s store -- is still a row somebody can find.

## A task, and the events that make it up

**The ledger's model is a task and the events inside it, in order.** A service's log lines lose
both: which task a line belongs to, and where among its lines it falls once several tasks
interleave. Here every event belongs to one task and carries its place in it.

A **task** is one thing a service was asked to do:

| Field                                   | What it is                                                           |
| --------------------------------------- | -------------------------------------------------------------------- |
| `service`, `id`                         | the service's name and its own id for the task                       |
| `kind`                                  | what was asked, in the service's words: `capture`                    |
| `state`                                 | `queued`, `running`, `done` or `failed`                              |
| `caller`                                | `public`, marked by the gateway, or `ours`                           |
| `parent`                                | the task that asked for this one, `{ service, id }`, when a task did |
| `asked_at`, `started_at`, `finished_at` | RFC 3339 instants, the last two once they happen                     |
| `summary`                               | a small JSON object the service chooses: for `shot`, the page's URL  |
| `detail`                                | why it failed, in the service's words, when it did                   |

An **event** is one step of a task, appended and never changed:

| Field                    | What it is                                                                            |
| ------------------------ | ------------------------------------------------------------------------------------- |
| `service`, `task`, `seq` | the task it belongs to, and its place there: a number that only grows within the task |
| `at`                     | when, an RFC 3339 instant                                                             |
| `stage`                  | the step, in the service's words: `resolving`, `loading`, `rendering`, `storing`      |
| `level`                  | `info`, `warn` or `error`                                                             |
| `message`                | one line for a person                                                                 |
| `data`                   | a small JSON object, when the step has figures worth keeping                          |

**Order is `seq`, never arrival.** The client numbers a task's events as the service makes them --
the moment in nanoseconds, or one past the task's last number when the clock has not moved on, so a
service restarted mid-history never reuses a number -- and so
a batch sent late, a retry, or two batches crossing change nothing about the order the ledger
answers with. A task's state changes are events too -- `queued`, `running`, `done`, `failed` as
stages -- and the task's row is kept in step with them in the same transaction.

**`parent` ties a chain together across services**: a scheduled job that asks `shot` for a capture
is the capture's parent, so one look follows the work from what started it to everything it
started.

## Pushed to, never asking

**A service tells the ledger; the ledger never asks a service.** `POST /events` takes a batch, each
item either a task -- its whole record, kept as the latest one sent -- or an event, kept once by its
`(service, task, seq)` so a batch sent twice is harmless. `PUT /tasks/{service}/{id}` stays for a
single task.

The ledger stamps a task's `updated_at` itself. **A later `asked_at` always replaces what is kept**:
it is the same task asked again, as `shot` asks a failed capture again under its id. Within one
asking, a record without a `finished_at`, or with an older one, does not replace one that has it,
so a late `running` cannot undo a `done`. An event of a later asking follows the earlier asking's
events in `seq`, since its numbers are later moments.

**Delivery is the service's to retry, and never its to wait on.** `libs/ledger` is the client every
Rust service uses: a task or an event is handed to it and the call returns at once; a background
task sends them in batches, and holds what could not be sent in a bounded queue, oldest dropped
first, trying again with backoff. A ledger that is down costs records, never a capture. **What it
holds or loses is said, once a streak**, since 2026-10-13, when a write's task looked lost and
nothing had said a word: the ledger not answering and answering again, a batch it refused with the
status it refused with, and the oldest dropped past the bound, each a line of counts and reasons,
never a record. **A service stopping flushes what it holds within a bound**, `Ledger::stopping`, which
flushes within `ledger::STOPPING`, five seconds inside the twenty host gives a container to stop,
and says how many items were left, so a redeploy no longer drops what was
waiting out a backoff. **The client names its TLS provider, a feature of the crate**:
`aws-lc-rs` by default, which the author chose on 2026-10-13 as running on x86_64 and arm64 alike,
and `ring` for a service whose own clients are built on it, so a build holds one; with both on,
aws-lc-rs is named, so a dependency that brings the other cannot leave it unable to choose.
**The gateway keeps nothing of the ledger**,
as of the registry: a list read through it was a quarter of an hour old.

**It is reached through Caddy, as a scope that is never public**: services write to
`api.internal.ixc.one/ledger`, which every container reaches, since Caddy admits Docker's private
range there. Like every scope it is on `api.canmi.app` too, where Access stands in front and our
Workers reach it over VPC; it is in the gateway's table as a private scope, so a node's private side
can send a write on to it from another node with `INTERNAL_TOKEN`, and the public never reaches it.
Nothing asks for a token, as nothing on the private side does; host is the exception, and the ledger
is not host.

## A run, and the node that replays it

**A run is a task that has a workflow**, the coordinator of RFC 23wgz. It is opened by `PUT
/v1/runs/{service}/{id}`, which makes the task, `queued`, and the run in one transaction, and its
task keeps the state the run is in: `running` at its first lease, `done` or `failed` at its end, an
event for each. Opening the same run again -- the same workflow, class and params -- answers `200`
and changes nothing; another run under its id is refused `409`. Only `class` `quorum`, the default,
is built; `local` is refused `400`. The registry's tasks stay one-event tasks until a controller
opens a run.

**What a node is leased is a turn**: the run, from the top. The node replays the workflow's code; a
`step.do` already done is answered with what it returned then, and one not done is run and
reported, until a sleep, a wait, a retry or the end of the code releases the lease. Only code knows
the next step's name, so the ledger leases the turn and cannot lease "the next step". A step's own
lease, for a `step.do` that declares where it runs, is not built; `steps` carries a nullable
`holder`, `fence` and `placement` so it adds no migration.

### The tables

Migrations 2 to 6, all expands, in the ledger's own style:

- **`runs`**, keyed `(service, id)` with a foreign key to `tasks`: `workflow`, `class`, `status`
  (`ready`, `running`, `sleeping`, `waiting`, `done`, `failed`), `params`, `output`, `error`,
  `run_at` (when it next needs a turn), `fence` (the last number granted, which only grows, and
  survives a lease let go) and its times.
- **`steps`**, keyed `(service, run, name)`: `kind` (`do`, `sleep`, `wait`), `status`, `attempt`
  (failures so far, counted toward the retries), `starts` (times begun, a lost holder included),
  `config` (the retries and timeout as declared, kept whole), `result` (JSON), `error`, `deadline`
  (this attempt's timeout), `wake_at`, and the unused `holder`, `fence`, `placement`.
- **`run_events`**, keyed `(service, run, key)`: what was sent to a run, kept once by the sender's
  key, with `taken_by`, the wait step that consumed it.
- **`leases`**, keyed `(service, run)`: `holder` (a node), `fence`, `expires_at`. A row exists only
  while a lease is held.
- **`executors`**, keyed `node`: `seen_at`, `capacity`, a nullable `push_url`, and `workflows`, the
  names of the workflows its last poll said it can run (migration 5).

A step's attempts are the task's events (`step.<name>`), so the console's timeline of a task already
draws them. Times are nanoseconds on the primary's own clock -- `clock_timestamp()` -- never a
client's, so one clock decides every expiry.

### The scheduler, the lease and the fence

**The scheduler runs in the ledger and acts only where `PATRONI_PRIMARY_URL`, its own node's
Patroni `/primary`, answers 200**: the node that leads the database schedules, and a failover moves
it with no election of ours. Without the variable a ledger schedules nothing, as on a copy that is
not one of the cores. A round, every second, is one transaction:

1. leases past `expires_at` are deleted and their runs made `ready`;
2. sleeping and waiting runs whose `run_at` has come are made `ready`;
3. `ready` runs are granted, oldest first, to the executors heard from in the last 60 seconds that
   have room (`capacity` less the leases they hold) and name the run's workflow, the one with the
   most room first; a run no executor with room names stays `ready` and takes no place in the
   round. The run's `fence` rises by one and a lease is written, all in one statement per step. Two rounds at once
   grant a run once, as `FOR UPDATE SKIP LOCKED` and the status test decide, so a second scheduler
   acting for a moment is harmless.

**Every write a node makes carries its `node` and `fence`, and is refused `409 stale_lease` unless
they are the run's lease now.** A holder that was lost and comes back writes under a number below
the run's and is refused; it may still have run a step a second time, which is the model.

**Leases last 30 seconds and are renewed every 10**, by `PUT /v1/runs/{service}/{id}/lease`. A
renewal and an executor's heartbeat commit `locally`, and so does the start of an attempt: losing
any of them only shortens a lease, hides a node for a moment, or counts a start less. A grant, a
step's result, a sleep, a wait, an event and a run's end are **confirmed by two of the three
cores** -- the default `synchronous_commit` under the cluster's `ANY 1`, RFC bhybp -- and answered
only then, so the next step begins once the last result is kept. Nothing here is a consensus of its
own.

### How a node gets a turn

**A grant is pushed and found by poll.** The round that grants a turn posts it to the executor's
`push_url` only when that executor is on the ledger's own node, `NODE`, which host sets on every
container: the private side reaches no other node's executor, since a scope placed on several nodes
goes to any of them, and a ledger without `NODE` pushes nothing. The post has a two second timeout,
and a failure is logged and nothing more. **A push is a doorbell**: the executor polls for the turn
it rings for, so a forged push can steer nothing -- RFC 23wgz. Every executor polls every 15 to 30
seconds -- `POST /v1/executors/{node}/polls` with its `capacity`, `push_url` and `workflows` -- which
is its heartbeat and answers the turns it holds, so a push lost, a node elsewhere, or one behind
NAT, still finds its work; a poll naming no workflow is refused `400 invalid_body`. A turn is
`{service, run, workflow, params, asked_at, fence, expires_at, done, partial}`, `asked_at` when
the run was opened, which never moves, so Workflows' `event.timestamp` is the same on every replay: `done` maps each done step's name to what it returned (a sleep's is
`null`, a wait's the event), up to 8 MiB, past which `partial` is true and the node reads the rest
with `GET /v1/runs/{service}/{id}`.

### The routes

All under `/v1`, in the envelope, on the private side as the rest of the ledger.

| Method and path                                   | Body                                                        | Answers                                                                                             |
| ------------------------------------------------- | ----------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| `PUT /runs/{service}/{id}`                        | `{workflow, class?, params?, caller?}`                      | the run view; `201` new, `200` same                                                                 |
| `GET /runs/{service}/{id}`                        | -                                                           | `{run, steps, lease}`                                                                               |
| `POST /executors/{node}/polls`                    | `{capacity, push_url?, workflows}`                          | `{turns}`                                                                                           |
| `PUT /runs/{service}/{id}/lease`                  | `{node, fence}`                                             | `{expires_at}`                                                                                      |
| `POST /runs/{service}/{id}/steps/{name}/attempts` | `{node, fence, config?}`                                    | `{status: "done", result}`, `{status: "failed", error}` or `{status: "started", attempt, deadline}` |
| `PUT /runs/{service}/{id}/steps/{name}`           | `{node, fence, attempt, outcome, result?, error?}`          | `{next: "continue"}`, `{next: "retry", run_at}` or `{next: "fail", error}`                          |
| `POST /runs/{service}/{id}/sleeps`                | `{node, fence, name, wake_at}`                              | `{status: "done"}` or `{status: "sleeping", wake_at}`                                               |
| `POST /runs/{service}/{id}/waits`                 | `{node, fence, name, type, timeout_ms}`                     | `{status: "done", event}`, `{status: "failed", error}` or `{status: "waiting", wake_at}`            |
| `POST /runs/{service}/{id}/events`                | `{key, type, payload?}`                                     | `{new}`                                                                                             |
| `POST /runs/{service}/{id}/stalls`                | `{node, fence}`                                             | `{status: "ready"}`, or `409 turn_stalled` on the third stall in five                               |
| `PUT /runs/{service}/{id}/outcome`                | `{node, fence, outcome: "done"\|"failed", output?, error?}` | `{}`; the same end said twice is the same                                                           |

`outcome` of a step is `success`, `error` or `fatal`. **The ledger's own codes are declared in
its `codes.json`**, never in lib's -- RFC v7g2p: `stale_lease`, `replay_diverged` and
`run_exists_differently` `409`, `result_too_large` `413`, `no_such_run` `404`, and `turn_stalled`
`409`.

**A run remembers how its last five turns ended**, `turns` on the run (migration 6): `released` for
one that let its lease go to a sleep, a wait or a retry, `stalled` for one the executor reported
stalled. A node lost, a lease lapsed and a `stale_lease` add nothing, so only the code's own doing
counts. A stall lets the lease go and makes the run `ready` again; **the third stall among the last
five fails the run and its task** `turn_stalled`, answered on that same write -- RFC 23wgz,
[executor.md](executor.md), "The limits".

### Workflows' API on these routes

- **`step.do`** asks for an attempt; a done step answers its result without running the callback, and
  an attempt not done is begun with a deadline of the step's timeout. **The ledger, not the node,
  decides what follows a failure**: an `error` with retries left makes the step `retrying`, sets the
  run `sleeping` until the delay has passed and lets its lease go, which ends the turn; one with
  none left, or a `fatal` one (`NonRetryableError`), makes the step `failed`, which the code may
  catch or let fail the run. The defaults are Workflows': `limit` 5 retries after the first attempt
  (at most 10,000), `delay` 10 seconds, `backoff` exponential -- `delay` times 2 to the failed
  attempt less one; `linear` and `constant` also -- and `timeout` 10 minutes, as its page
  https://developers.cloudflare.com/workflows/build/sleeping-and-retrying/ gives them.
- **A node lost mid-step is not a failed attempt.** A step found `started` by a new holder is the same
  attempt begun again, counted in `starts` but not against the retries; a step begun 20 times by
  holders that were lost fails. A step found `started` past its `deadline` is a failed attempt.
- **A result is at most 1 MiB of JSON**; a larger one fails the step and is answered `413`.
- **`step.sleep` and `sleepUntil`** are one route with an absolute `wake_at`, fixed by the first turn
  that reached it and kept in the step, so a replay sees the same instant. The run sleeps holding no
  lease and no node.
- **`step.waitForEvent`** takes the oldest event of its `type` not yet taken, or the run waits holding
  nothing. An event sent to a waiting run for a type it waits for makes it `ready` at once; at its
  timeout with none taken the step fails, as Workflows throws.
- **A replay that disagrees with the record** -- a step name reused as another kind -- is refused
  `409 replay_diverged` rather than answered wrongly.

### Tested with an executor of its own

`src/harness.rs` is the executors' half in the shape of Workflows' `step`: a workflow is a closure
over `run`, `sleep`, `sleep_until` and `wait_for_event`, replayed through the routes, with a node
that can be lost at a step and one that takes pushed turns. The tests run against a real Postgres
and move time by SQL, never by waiting: a replay runs a done step once, a lost node's late report is
refused under the next fence, retries back off and stop at their limit, a crash is not an attempt
and twenty starts fail, a sleep and a wait hold no lease, two rounds grant a run once, a push lost
costs nothing.

## Read by the console

`GET /tasks` lists tasks newest first, a page at a time -- each item carries its `cursor`, and
`before=<cursor>` of the last one asks for the next page, `limit` at most 500 -- narrowed by
`service`, `state`, `kind`, `caller` or `parent`; `GET /tasks/{service}/{id}` is one, with its
events in `seq` order and the tasks it is the parent of. The console's Tasks page is to read these:
a list, and a task as a timeline -- each step, how long it took, where it failed, what started it
and what it started. The answer is always the envelope. A query that does not read is forgiven
rather than refused: a cursor that does not parse starts from the top, a filter naming no known
value matches nothing, and a limit out of range is brought into it.

**The list is ours alone.** A record carries what somebody asked for -- a capture's URL is someone
else's browsing -- so no route of a public scope lists records; a public caller reads its own task
by the id it was given, from the service that made it.

## Counted for telemetry

`GET /counts?hours=` answers how many tasks each service was asked for in each of the last `hours`
whole hours and the one under way -- 1 to 168, 24 when absent -- by `asked_at`, as
`[{ service, hour, state, count }]`, `hour` the RFC 3339 instant it starts, oldest first. It says
nothing of a task but its service, when it was asked and how it stands, which is what `telemetry`
may publish. See [telemetry.md](telemetry.md).

## Kept for good, in the platform's Postgres

**Nothing is ever deleted, tasks or events.** A task is a few hundred bytes and an event less; a
year of captures at thousands a day, each a handful of events, is some hundreds of megabytes.

**The ledger's history is `platform_ledger` in the platform's cluster**, owned by the `platform`
role and reached by the `DATABASE_URL` that `mise run database grant platform ledger` writes --
[databases.md](databases.md), "The platform's alone, and its scope a role in it". So it is backed
up, mirrored and failed over as the cluster is, and the ledger keeps nothing on its node.

- **Two tables on their natural keys**: tasks keyed by `service` and `id`, events by `service`,
  `task` and `seq`, so no key is ever minted. Times are nanoseconds in a `bigint`, since the cursor
  and its ties need more than `timestamptz`'s microseconds; a record and an event's data are
  `jsonb`; the key columns are `COLLATE "C"`, so the order is bytewise whatever the cluster's
  locale.
- **Which write wins is decided in SQL**, one `INSERT ... ON CONFLICT DO UPDATE ... WHERE` per task,
  so it holds however many ledgers write at once; a batch is still one transaction.
- **Its schema moves by migrations the ledger runs itself**, each at start under one advisory lock,
  in its own transaction with its row in `ledger_migrations`. A migration is an expand or a
  contract, and a test refuses an expand that drops, renames, retypes, adds `NOT NULL` or truncates
  anything; a runner for every app is RFC hz1av.
- **Its SQL is tested against a real Postgres 18**: a service in CI, and a throwaway one in docker
  locally; without one the SQL tests say so and pass over themselves, and in CI that is a failure.
- **The SQLite it was kept in is read once, by `ledger import <path>`**, which copies every task as
  kept and every event, any schema it ever had, and copies nothing twice when run again.

Until it moves in, the ledger deployed on `rdu` is the SQLite one, `ledger.db` in its own directory,
and its rollout is by hand so the new one is not deployed before its database exists -- RFC xsarw.
