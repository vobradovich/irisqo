# Flow control v1

How irisqo limits how many jobs of one key run at once and how fast they start. The tables and the SQL are in
[`sql.md`](sql.md); the job fields are in [`json-v1.md`](json-v1.md).

## Flows

A **flow** is a key on a job, `flow_key`. It limits concurrency and start rate; it **promises no order**. Order and
dependencies between jobs are not a flow's job.

- A `flow_key` is 1-128 characters `a-z 0-9 . _ : -`, the first a letter or a digit (`payments`,
  `host:api.example.com`, `order-42`). A job without one has no flow.
- A **declared** flow has a row in `flows` with its settings:

  | field | meaning | default |
  |---|---|---|
  | `concurrency` | jobs of the flow running at once, 1-100000 | 1 |
  | `rate` | starts per `rate_period`, 1-1000000 | no limit |
  | `rate_period` | whole seconds, 1-31536000 | 1 |
  | `paused` | the flow starts nothing; set by `…/pause` and `…/resume` | false |

- An **undeclared** flow is any other name. It has no row and the defaults: concurrency 1, no rate, never paused. This
  is mutual exclusion per key, e.g. one job per order at a time.

The class of a name is found at each claim, not stored on the job. Declaring or removing a name with waiting jobs
changes nothing else: the next claim treats it by its new class.

## API

| request | what it does |
|---|---|
| `PUT /api/v1/flows/{id}` | declares a flow or replaces its limits; the pause and the spent part of the rate stay |
| `GET /api/v1/flows/{id}` | one declared flow |
| `GET /api/v1/flows` | a page of declared flows by name, with `waiting`, `processing` and `rate_left` |
| `DELETE /api/v1/flows/{id}` | removes the declaration; the name becomes undeclared |
| `POST /api/v1/flows/{id}/pause`, `…/resume` | stops or resumes the starts of a declared flow |
| `GET /api/v1/undeclared-flows` | a page of the names that waiting or running jobs use without a declaration |

A job gets its flow from `flow_key` (`POST /api/v1/jobs`, `/to/?_flow_key=`, `enqueue_v1`, a callback). The UI has a
Flows page (declared, undeclared, all) with the waiting and processing jobs of each flow, the rate (`10 per 1 min · 6
left`) and, when a rate holds jobs back, the time of the next free start.

A change of a declared flow (put, pause, resume, delete) takes the same advisory lock of the flow as a claim, but
waits for it (`pg_advisory_xact_lock`) until its commit. A claim therefore sees the limits either before or after a
change, never a mix of the two.

## Where a job waits

A job is in one of the state tables `scheduled -> enqueued -> claimed -> processed`. `flow_key` and `priority` are
copied along, so a release or a retry puts the job back into its flow.

- `scheduled`: not due yet (`delay`, `at`, runs of a schedule, every retry with a delay).
- `enqueued`: due. Every row here can start as soon as its flow allows it.
- `claimed`: running. The rows of a flow here are its running jobs; `concurrency` is counted from them.

## The claim

A worker instance calls `claim_v1` for a full batch (`--workers × --prefetch`) in one transaction. The batch is
filled from three classes:

```text
claim_v1
├── jobs without a flow          claim_plain_v1
└── jobs of flows                claim_flow_v1
    ├── declared flows           claim_declared_ids_v1
    └── undeclared flows         claim_undeclared_ids_v1
```

### Turns between the classes

Each claim picks by chance which part goes first; the other part gets the places that are left.

- With the chance `p_flow_share` (25 % by default) the flows go first, otherwise the jobs without a flow. The flows
  go first only when a job of a flow is waiting, so a queue without flows pays nothing for them.
- Inside the flows, with the chance `p_declared_share` (50 % by default) the declared flows go first, otherwise the
  undeclared ones.

So a backlog of one class does not starve another: the flows get the first pick in about one claim of four even
while the jobs without a flow would fill every batch, and an undeclared flow is not starved by a declared one with a
large `concurrency`. Both shares are parameters of `claim_v1`; the service uses the defaults.

### Jobs without a flow

`priority DESC`, then `attempt` (new jobs before retries), then `id`, with `FOR UPDATE SKIP LOCKED`. One statement,
the fast path of most claims.

### Declared flows

A declared flow is **ready** when it is not paused, has a free start by its rate, has waiting jobs and has
`running < concurrency`.

1. The ready flows are locked with `pg_try_advisory_xact_lock`, the least loaded first (`running / concurrency`, ties
   by chance), at most one flow per place. A flow locked by another claim is skipped.
2. A new statement (a fresh snapshot, after the locks) reads `running` and the rate again. Each locked flow offers up
   to `least(concurrency - running, remaining)` jobs (`priority DESC`, `attempt`, `id`). The places are filled **by
   water level**: the `k`-th job of a flow is ranked by the load the flow would have with it, `(running + k) /
   concurrency`, and the lowest ranks win. With 100 and 10 free places and 64 to give, the flows get 59 and 5.

### Undeclared flows

1. The distinct waiting names are walked over `ix_enqueued_flow` (one index probe per name) **from a cursor**: first
   the names after it, then from the start up to it. A claim looks at 4 names per place at most (256 for a batch of
   64), so its cost does not grow with the number of busy names. A name is kept when it has no row in `flows` and no
   job in `claimed`. The walk is lazy and tries `pg_try_advisory_xact_lock` on each kept name until it holds one per
   place; a name locked by another claim is skipped and the walk goes on.
2. A new statement takes the head job of each locked name (`priority DESC`, `attempt`, `id`), again only when the name
   has no row in `flows` and no job in `claimed`.

The cursor is where the walk stopped: the last locked name when the places ran out, otherwise the last name looked at.
It is kept per instance in `instances.flow_cursor`, written by the claim only when it moves, `NULL` for a new instance.
The row is written by the heartbeat of the instance at its start; a claim for an `instance_id` without a row walks
from the start every time.
An empty claim moves it too, so a free name behind thousands of busy ones is reached within a few claims, and a name
waits about (ready names / batch) claims, not behind every name before it in the alphabet.

### One statement claims the chosen jobs

After both flow functions chose ids, one statement deletes them from `enqueued`, writes `history`, inserts them into
`claimed` with the lease `now() + p_lease`, moves `next_at` of every rate-limited flow by the number of its starts,
and returns the jobs (`priority DESC`, `attempt`, `id`). The advisory locks are held until the claim commits, so no
other claim fills the same free place.

## Rate (GCRA)

The rate is a bucket kept in one timestamp, `next_at`: the time by which the starts so far are paid off.

- One start costs `T = rate_period / rate`.
- The debt is `D = max(next_at - now(), 0)`; `NULL` is no debt.
- Free starts right now: `remaining = floor((rate_period - D) × rate / rate_period)`.
- A flow is ready when `remaining >= 1` and takes at most `remaining` jobs.
- A claim of `n` jobs sets `next_at = greatest(next_at, now()) + n × T`.

After an idle period a flow starts up to `rate` jobs at once (within `concurrency`), then one more every `T`. Within
any window of one period at most about `2 × rate` jobs start; the long-run rate is `rate / rate_period`.

A new declaration keeps the spent share of the bucket: `next_at = now() + D × new_period / old_period` (`NULL` without
a rate). A repeated `PUT` changes nothing; a new rate refills at its own pace.

`flow_rate_free_v1(rate, rate_period, next_at)` computes `remaining` for both the claim and the flow list, so the UI and
the claim agree.

## Priority

`priority` orders the jobs inside a flow, and the jobs without a flow among themselves. It does not rank a flow against
other flows or against the jobs without a flow: those take turns as described above.

## Release and retry

- A retry without a delay and a release (`unlock`): `claimed -> enqueued`, `attempt + 1`.
- A retry with a delay: `claimed -> scheduled`, then `enqueued` when due.
- A stop of the instance (`kill`) puts its jobs back into `enqueued` with the same attempt; an ended lease
  (`claimed.until`) puts them back with `attempt + 1`, so a late result is rejected.

A job that goes back loses its place: it waits again among the jobs of its flow by `priority`, `attempt`, `id`.

## What is and is not guaranteed

- A declared flow runs at most `concurrency` jobs at once and starts at most what its rate allows.
- An undeclared flow runs at most one job at a time.
- A paused flow starts nothing. A pause that commits between the two steps of a claim may let that one claim start
  jobs; the next claim sees it.
- Removing a declaration while more than one job of the flow runs: the name gets no new job until they all end.
- No order within a flow beyond its priority; a retry or a release goes back with its place lost.
- A claimed job holds its place from the claim on, including while it waits in the instance's channel.

## Observability

- `irisqo.job.queue.duration{job.flow}` (histogram, s): how long each claimed job waited from due (`enqueued.at`) to
  its claim, by class: `none`, `declared`, `undeclared`. It is recorded at the claim, per job, with no query of its
  own. The percentiles of the classes side by side show starvation and the effect of the shares. A job of a
  rate-limited or busy flow waits by design, so the `declared` and `undeclared` tails include the limits, not only a
  lack of workers. No metric is labelled by flow name: the number of flows is not bounded.
- Stats: `held` counts the jobs of `enqueued` that belong to a flow; `enqueued - held` wait for a worker. `lag_seconds`
  ignores the held jobs, since a flow keeps its jobs back by design.
- Gauges: `irisqo.queue.depth{state=enqueued}`, `irisqo.queue.held`, `irisqo.queue.lag`.
- The Flows page and `GET /api/v1/flows` show waiting and processing jobs per flow and the free starts (`rate_left`).
