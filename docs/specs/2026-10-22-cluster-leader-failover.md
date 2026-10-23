# Cluster leader failover

**Date:** 2026-10-22
**Status:** Waiting on operator decisions

## What this is

One orch process per machine. One of them is the leader. The leader routes tasks, claims them, reviews, merges, and creates job issues. The other machines are workers. They run agents. When the leader goes offline, a worker becomes the leader with no manual step.

Every node can show who is leader, how fresh the others are, and which tasks are running where.

## Why

Each machine keeps its own SQLite store and writes GitHub labels without reading them back. Ingest copies a status label the first time it sees a task (`src/engine/sync.rs`), so one machine does not learn the other's later changes. Two machines on one project then dispatch the same task, force-push the same branch, reset each other's sessions, merge a head that just moved, and create the job issue twice. Attempt counters, review cycles, and cooldowns are per store, so the limits apply twice. Internal task ids are SQLite row ids, so the two stores reuse the same `internal:N` values.

## Third vote

Two nodes have no majority. If the link between them drops, each side sees the other as dead and takes the work. Consensus needs `N/2+1` votes. A safe failover needs a third vote. That vote can be a full node, a witness that runs no agents, or an external compare-and-swap.

## Options

Who decides, where the state lives, and how nodes talk are three separate choices.

| Leader election | Third vote | Cost |
|---|---|---|
| Embedded Raft (openraft). The Raft leader is the orch leader. Store writes go through the Raft log. | A witness node running orch with no agents | New dependency. Every store write waits for a majority. openraft 0.9 is the stable line. 0.10 is still alpha. |
| NATS JetStream KV compare-and-swap on a lease key. Task events go through a stream. | The NATS server. One server is a single point of failure. Three servers are a cluster. | Another service to run. New dependency (`async-nats`). |
| Object in an S3-compatible bucket. Renew with `If-Match`, create with `If-None-Match: *`. A losing write returns 412. | The bucket | Cloud account and credentials on every node. The provider has to support conditional writes. |

| State | How a new leader gets it | Cost |
|---|---|---|
| Raft log, with embedded Raft | The new leader already has the log | No separate handover |
| Leader SQLite plus a change stream, with NATS or the bucket | Workers apply the stream and take snapshots. On promotion, reconcile labels, open PRs, and branches with GitHub, and ask workers which sessions are alive | A replica can lag. Reconciliation is where the bugs will be |
| rqlite or dqlite | Replicated SQLite | rqlite is another process. dqlite is a C library. Both still need three nodes |

| Transport | How nodes talk | Cost |
|---|---|---|
| Tailscale on the machine | Listen on the tailnet address only | Changes the settled "No external endpoints" rule. Needs an operator decision, and the rule text has to change with it |
| iroh | QUIC dialed by public key, with a relay if hole punching fails | New dependency. The public relays are run by someone else unless we host them |
| tailscale-rs | Tailscale inside the process | Pre-alpha |

## Rejected

GitHub as the lock. Labels and assignees have no compare-and-swap. A ref lease ties the cluster to GitHub uptime and the rate limit, and a ref under `refs/heads/` fires push events. Every renewal is an API write.

Two peers and no third vote. A partition becomes split brain.

A shared SQLite file on a network filesystem. SQLite locking there is unsafe.

A fixed leader with no failover. The cluster stops when that machine stops.

Last-write-wins (Corrosion, cr-sqlite) for claims. It cannot give one node exclusive ownership of a task. It would be enough for a read-only monitor.

Sharding with `host:a` and `host:b`. No races, no failover, no shared view. Usable as a stopgap if each node also skips ingest outside its shard.

## Recommendation

Embedded Raft over Tailscale, with two workers and one witness. The state and the leader come from the same log, so there is no replica to hand over. The witness runs no agents. Raft already exposes leader, term, replication lag, and last contact.

If a third box is unacceptable, use the bucket lease and the streamed replica. The bucket is the third vote. The replica is the handover.

## Guards that do not need a cluster

These landed with this spec. They hold for one machine and for two.

`rebase_on_default` writes `orch-push-lease` in the worktree git dir before it fetches. The file records `origin/<branch>`, or `absent` when that ref does not exist. A later call leaves that value in place, so a retry does not adopt a tip it has only fetched. `push_branch` and the auto-merge rebase push pass that sha to `--force-with-lease=<branch>:<sha>`. A fetch that learns another machine's commits does not move the lease. The force push fails, and the existing diverged-branch path rebases onto the remote tip instead of replacing it. After that rebase, the lease moves to the tip that was just integrated. A successful push moves it to the new `HEAD`.

`merge_pr` sends the head sha CI was checked against. GitHub returns 409 when the head has moved. The next tick reads the new head and checks CI on it. The rebase-and-force-push path stays for an out-of-date base branch.

## Still required once a leader exists

Fencing. Every command from the leader carries the term. Workers reject an older term. A leader that cannot reach a majority stops dispatching before the lease runs out.

Recovery asks the node that owns the task whether its session is alive. A node whose heartbeat times out has its tasks moved back to `routed`. Shutdown on a worker touches that worker's sessions only.

Each task records the node that runs it.

`orch nodes` shows role, term, last heartbeat, connection, and running tasks. Worker output reaches the leader's event bus with the node id, so `orch stream` shows every machine.

## Costs

A tailnet listener changes "No external endpoints" in `AGENTS.md`. That rule stays until the operator changes it.

Embedded Raft adds a dependency and puts every store write through consensus. A write takes one round trip to a majority.

The runner splits into decide and run. The tmux runner stays.

Failover pauses dispatch for one election timeout.

openraft, async-nats, and iroh need a vetting PR before this repo depends on them.

## Decisions still open

Tracked in #3728, with `no-agent` so it is not dispatched.

- Witness box, or an external third vote (NATS or object storage).
- One cluster for every project, or one cluster per project.
- Election timeout and lease length.
- Whether the tailnet listener exception is accepted. Any node can answer `orch nodes` from its own Raft state, including after the old leader is gone.

## PR plan

1. **Pass an expected sha on force-push and merge.** This change. Files: `src/engine/runner/git_ops.rs`, `src/engine/auto_merge.rs`, `src/engine/sync.rs`, `src/github/http.rs`. No dependency.
2. **Vet the crate the operator picks.** Maintainer PR. No orch behavior change.
3. **Leader election and fencing.** Term on every worker command. Dispatch stops when the lease is lost. Depends on 2 and on the tailnet decision.
4. **Replicated task state.** Raft log, or leader SQLite plus a stream. Depends on 3. One cluster scope (all projects, or one per project) is fixed here.
5. **Worker run protocol.** The leader claims a task and tells a worker to run it. The worker reports the session. Depends on 4.
6. **Recovery and shutdown use the owner.** `src/engine/tick.rs`, `src/engine/sync.rs`, `src/engine/mod.rs`. Depends on 5.
7. **`orch nodes` and multi-machine `orch stream`.** Depends on 4 and 5.
