# `executor`: the node that replays a run

`apps/compute/executor` is the node's half of RFC 23wgz: the ledger coordinates a run and keeps
everything of it, and an executor replays it, a turn at a time. Its decisions are the RFC's last
bullet, "The second slice, the executor"; the ledger's half is [ledger.md](ledger.md), "A run, and
the node that replays it".

## A runner and a driver, cut where workerd will cut them

**The executor is two halves, split along the line RFC l4s6a draws.** The runner, `libs/replay`,
replays one turn. The driver, `apps/compute/executor`, is the resident part of a node: it polls,
answers the doorbell, renews leases, keeps to its capacity, and hands each turn to a runner. Once
workerd runs functions, the runner moves into an isolate per turn and the driver stays a resident
service; nothing else moves.

**What must stay the same, so that move is free:**

- **A workflow's source.** It is written in Cloudflare Workflows' API and imports its names from
  workerd: `WorkflowEntrypoint` from `cloudflare:workers`, `NonRetryableError` from
  `cloudflare:workflows`. Where workerd does not provide them, `libs/replay/src/cloudflare.ts`
  does: `tsconfig.json` maps both names for the type checker, `vitest.config.ts` for the tests, and
  a resolve hook in the turn's thread for Node. The runner imports the same names, so a workflow's
  `NonRetryableError` is the runner's in either runtime.
- **A workflow reaches a service through `this.env` alone**, never a hostname. See "Bindings".
- **The runner uses web APIs alone**: `fetch`, timers, `AbortSignal`, never a `node:` module, and
  only TypeScript Node can run by stripping its types. `libs/replay/src/web.test.ts` holds both.
- **Between the driver and the runner passes plain data only**: the turn, the node, the ledger's
  base and the bindings in; how the turn ended out. Today a thread's message, later a call into the
  isolate.

## How a turn runs

**A turn is the run replayed from the top.** A step the ledger already holds is answered from the
turn without its callback running; one it does not is asked for, run and reported, until the code
returns, throws, or a step ends the turn -- a sleep, a wait, a retry. The code returning reports the
run `done` with its output, and throwing reports it `failed`. A replay the ledger refuses
`replay_diverged` reports the run `failed`, that word first in its error; any other end of a turn
reports nothing and leaves the run to the next.

**A step that ends the turn returns a promise that never settles**, so no `try` in the workflow
catches the halt. Once the turn has ended no step asks the ledger again, and the turn ends when
none is still asking, so steps awaited together each have their answer first. A step that failed
for good throws an `Error` the code may catch, as Workflows does.

**A result is returned as JSON reads it back**, the same in the turn that ran it and in every
replay: `undefined` is `null`, a `Date` a string. Workflows' `event` is the run's `params` as
`payload`, its id as `instanceId`, and `asked_at` as `timestamp`, the same instant in every turn. A
rollback handler and a delay given as a function are refused, since the ledger keeps neither;
`sensitive` is taken and changes nothing.

## The limits

- **A step's callback has at most 5 minutes**, RFC l4s6a's limit, already under Node: a `timeout`
  above it is held to it, and Workflows' default of 10 minutes reads as 5. A callback past its
  timeout is a failed attempt, and a `NonRetryableError` is reported `fatal`. Work longer than that
  starts asynchronously and is waited for with `step.waitForEvent`.
- **A turn's heap is at most 128 MB**, l4s6a's limit for an isolate.
- **A lease lasts 30 seconds and is renewed every 10**, by the driver, for as long as its turn runs.

## A thread stands in for an isolate

**Each turn runs in a `worker_threads` Worker of its own**, made for the turn and terminated after
it: a fresh global scope, so code that keeps state between turns fails here first rather than in
workerd; a heap of 128 MB; and `terminate()` as the hard stop. A thread that runs out of heap,
throws past the runner, or is terminated is a lost holder: nothing is reported, the lease runs out,
and another turn takes the run under the next fence.

## Polls, and a push that is a doorbell

**The driver polls at start and then every 15 to 30 seconds, at random**, so executors started
together do not poll together, with its `capacity`, its `push_url` and `workflows`, the names it
can run. A poll answers the turns it holds. Each run is begun once however often a poll answers it,
and a newer fence of a run stops the older turn and begins the newer.

**A push is a doorbell.** `POST /turns` only polls, and whatever it carries is ignored: anyone on
the private side can post there, and a forged turn's `done` would steer a replay. The ledger pushes
only to the executor on its own node; the rest find theirs by polling. A ring during a poll polls
once more after it, so a grant made meanwhile is not left for the timer.

**A lease is renewed every 10 seconds while its turn runs.** A renewal refused `stale_lease` stops
the turn at once; one not answered keeps it until the lease has run out by the last expiry the
ledger gave, then stops it.

## Capacity

**`capacity` is how many leases the executor holds at once**, turns and placed steps together,
each a thread. It is named in every poll, and the ledger grants no more than it leaves room for; a
turn a poll answers past it is logged and left for its lease to run out. A thread's 128 MB is a
ceiling, never a reservation -- RFC fr1vj.

## Bindings

**A workflow's `env` is its bindings, each a `fetch`** that sends the path and query it is asked
for to the base it was declared with, whatever host the request named, as a workerd service binding
does. Every workflow is given `LEDGER`, the URL map's ledger, and `REGISTRY`, the registry's scope,
both on the private side; `BINDINGS` adds or replaces them. What only a server can do is offered to
a workflow the same way, as a binding -- RFC dmmk8.

## Where a workflow lives

**In the executor, `workflows/<service>/<name>.ts`, named in `workflows/index.ts`** by the name a
run is opened with, until l4s6a's deployments carry a function's code apart. Each is a module and
the class it exports, loaded only by the thread replaying it, so the driver never loads a workflow.
Only test workflows exist until `mirror`: `echo`, one step, and `nap`, a step, a sleep and a step.

## Configuration

From its environment, as every platform service reads its own:

| Variable     | What it is                                                                                  |
| ------------ | ------------------------------------------------------------------------------------------- |
| `LISTEN`     | where the doorbell and `/health` answer, `host:port`; `0.0.0.0:12030` when unset            |
| `NODE`       | the node it runs on, which host sets on every container; without it, it does not start      |
| `CAPACITY`   | how many leases it holds at once, a whole number from 1; 4 when unset                       |
| `LEDGER_URL` | the ledger's base; the URL map's, on the private side, when unset                           |
| `PUSH_URL`   | where the ledger rings it; its own scope on the private side, `/executor/turns`, when unset |
| `BINDINGS`   | `NAME=url` pairs separated by commas, added to or replacing the default bindings            |

`/health` answers its node, capacity, the turns running and when a poll was last answered, and
answers `200` whether the ledger is reached or not: host reads health only to call a deploy good.

## Stopping

**On SIGTERM the executor stops polling and gives its running turns 15 seconds**, inside the 20
host gives a container, then terminates what is left. A turn cut off is a lost holder, which the
model is built for.
