# Multi-machine cluster design

- Date: 2026-10-26
- Status: Proposed
- Issues: #3727 (cluster), #3728 (decisions), #3729 (single-instance guards)

## What this is

Orch on one or more machines, as one cluster. Every machine runs the same binary. One node is the leader and makes every decision: routing, claims, recovery, review and merge, jobs, and the GitHub poll. Every node that has a project checked out can run its agents. When the leader goes offline, another node takes over with no manual step. One machine stays the default and behaves like orch today.

## Problem

Orch assumes one instance per machine and per repo. Two instances on the same project race on almost every transition, because each treats its own SQLite store as the truth and never reads the other's changes. Ingest copies a status label only the first time it sees an issue (`src/engine/sync.rs:3398-3402`).

| Race | Where | Result |
|---|---|---|
| Both dispatch one task | `tick_route_tasks` and `dispatch_tasks_for_repo` in `src/engine/tick.rs` | Two agents on one issue |
| Same branch pushed from both | `branch_name` in `src/engine/runner/worktree.rs`, `push_branch` in `src/engine/runner/git_ops.rs` | Each machine overwrites the other's commits |
| One machine resets the other's running task | `tick_recover_stuck_tasks` in `src/engine/tick.rs`, recovery in `sync_tick` in `src/engine/sync.rs` | Recovery checks local tmux only |
| Shutdown resets the other's tasks | graceful shutdown in `serve` in `src/engine/mod.rs` | A restart moves labels of tasks another machine runs |
| Merge and re-route at once | `auto_merge_pr` in `src/engine/auto_merge.rs`, `merge_pr` in `src/github/http.rs` | Agent dispatched to a merged branch, or a merge of a head just force-pushed |
| Duplicate reviews | `spawn` in `src/engine/subscribers/review.rs` | The guard is in-process only |
| Duplicate job issues | `execute_job` in `src/engine/jobs.rs` | Each machine creates the issue |
| Split counters | `max_attempts`, `review_cycles`, cooldowns | Each machine enforces its own limit |
| `internal:N` collisions | ids are SQLite row ids | Two sets of internal tasks with the same ids |

The only single-instance guard today is `kill_other_orch_serve_processes` (`src/home.rs:153`), which works on one machine only.

## Why three votes

Two nodes have no majority. If the link between them drops, each side sees the other as dead and takes the work. Consensus needs `N/2+1` votes, so failover needs a third vote: a full node, a witness that runs no agents, or an external compare-and-swap service.

## Decision

Embedded Raft ([openraft](https://github.com/databendlabs/openraft) 0.9) between orch nodes. Only coordination state goes through the Raft log. The leader decides and every node runs agents. The third vote is an always-on node, either a full voter or a witness.

| Question from #3728 | Answer |
|---|---|
| Where the third vote lives | An always-on node as a voter. A witness voter runs orch with no agents. No external service |
| One cluster or one per project | One cluster for every project. Orch already runs one daemon over a single shared database (`src/engine/mod.rs:667-669`) |
| Timeouts | Heartbeat 250 ms, election timeout 1 to 2 s, task lease about 90 s renewed every 30 s. All configurable |
| Network listener | Off by default. With a `cluster` section it binds only to the node's Tailscale address, and every request carries a cluster secret. Nodes find each other through `tailscale status`. "No external endpoints" changes to allow this one opt-in listener |

## Options considered

| Leader election | Third vote | Why not chosen |
|---|---|---|
| NATS JetStream lease (KV compare-and-swap) | the NATS server | One more service to run, and one server is a single point of failure. State still needs a separate replica |
| Object storage lease (`If-Match` on the etag) | the bucket | Cloud credentials on every node. State still needs a separate replica and a reconcile step on promotion |
| GitHub ref lease | GitHub | Ties cluster health to GitHub uptime and the rate limit, every renewal is an API write, and a ref under `refs/heads/` fires push events |

| State | Why not chosen |
|---|---|
| Replicate all of SQLite (rqlite, dqlite, hiqlite) | Copies every table, including the heavy ones, and replicating SQL statements breaks on `now()` and random values |
| Leader SQLite plus a streamed replica | A replica can lag, and the reconcile step on promotion is where the bugs would be |
| CRDT SQLite (Corrosion, cr-sqlite) | Last-write-wins cannot give one node exclusive ownership of a task |
| GitHub labels as the truth | No compare-and-swap, rate limits, and slow reads |

| Transport | Why not chosen |
|---|---|
| iroh | New dependency, and its public relays are run by a third party unless self-hosted |
| tailscale-rs | Pre-alpha |

Also rejected: two peers with no third vote (split brain), a shared SQLite file on a network filesystem (unsafe locking), a fixed leader with no failover, and sharding by host label (no failover, no shared view).

## Source of truth

SQLite is already authoritative for task status. `TaskManager::update_task_status` writes the store first and mirrors the `status:*` label as best effort (`src/engine/tasks.rs:488`). GitHub is the source for what happens on GitHub: new issues, title and body edits, comments and mentions, PR state, reviews and CI.

The cluster keeps that model and replaces the local store with a replicated one for coordination state. GitHub stays an input for those events and an output the leader writes to in the background. Orch never reads its own decisions back from GitHub.

## What goes through Raft

Orch writes tens of rows a minute and nothing per agent output line. The heavy payloads are `task_runs` (prompts and stdout) and `upsert_external`, which rewrites every open issue on each sync. Those stay local.

| Replicated | Local to each node |
|---|---|
| `tasks`: status, routing, counters (`attempts`, `review_cycles` and the rest), `block_reason`, `pr_number`, `parent_id`, and new `owner_node`, `lease_until`, `term` | `task_runs`, `task_metrics`, `rate_limits`, `control_messages`, `metrics:*` keys |
| `task_dependencies`, `job_state`, `channel_subscriptions` | `tasks.worktree`, `review_session_expected`, token and cost columns |
| `kv` coordination keys: cooldowns and failure counts, approved SHAs, `ci_pending_since`, `github:rate_limited_until`, ingest cursors, `control:*` | tmux sessions, worktrees, `output.json`, `orch.log` |
| Node registry, progress records, GitHub outbox, and an internal id counter | `task_activity`, written by each node when it applies a status change |

Issue title and body are replicated so a worker never calls GitHub to start a task. They are written only when they change, and only while the task is active. When a task reaches `done`, the replicated row shrinks to a tombstone: id, repo, status, `pr_number` and the time. The full row with its body stays in the local database of each node that had it. The tombstone keeps `done` terminal and resolves dependencies on that task.

Measured on the current single-machine database: `orch.db` is 4.0 GB. 16 tasks are active, with about 60 KB of title and body. 6,333 tasks are done, with 29 MB of title and body. Non-metric kv rows total 194 KB. With tombstones, the replicated state is under 1 MB, so a new node catches up from one snapshot in under a second.

The in-memory guards (the `dispatching` map in `src/engine/dispatch_guard.rs`, the cooldown map in `src/engine/cooldown.rs`) stay on the leader. They only guard decisions, and a new leader rebuilds them from the replicated state.

## How it fits the store

`TaskStore` is the only write path: "All task state, metrics, KV, and rate limits flow through this module" (`src/store/mod.rs:9`). Raw SQL outside it is in tests only. The engine call sites stay as they are. The write methods for the replicated tables propose a Raft command, and apply runs that command against SQLite on every node, so reads stay local.

- Commands mirror the store methods. `update_status_if` (`src/store/tasks.rs:1009`) and `kv_insert_if_absent` (`src/store/kv.rs:60`) are already compare-and-swap, and as Raft commands they hold across machines.
- Apply is deterministic: the proposer stamps the time, and ids are allocated inside apply.
- A snapshot is the replicated rows serialized as JSON. Install deletes and inserts those rows in one transaction and never touches local columns or tables. This is the approach in `../agents-coordination` (`src/store.rs`). It works because the replicated state is under 1 MB. Rejected: copying the SQLite file and swapping it in, which would overwrite local tables.
- The Raft log and vote live in their own file, `~/.orch/raft.db`, so orch's migrations never touch them.
- With one node, a write commits locally, so a single-machine install pays no network cost.

The CLI opens the database directly (`src/cli/mod.rs:766`, `:772`, about 33 write sites). Those writes move to the local daemon over the existing localhost websocket, so they also go through Raft.

## Leader and workers

Only the Raft leader runs the decide phases of the tick and sync loops. One node polls GitHub for the whole cluster.

Each node watches the replicated state for tasks assigned to it, runs them in tmux, renews a lease, and reports completion as a Raft command. The log carries the messages, so there is no separate leader to worker protocol. Review runs are assigned to workers like any agent run.

Fencing:

- Every assignment carries the leader's term, and workers reject an older term.
- Before a GitHub side effect, the leader confirms it still has a majority (`ensure_linearizable`).
- Merges send the head SHA (#3729).

GitHub writes go through an outbox in the Raft state. The leader records a label, comment or merge as pending, runs it, and marks it done. A new leader retries what is pending, so every outbox operation must be idempotent. The rate limit state is shared, so a 429 pauses every node.

Recovery checks the owner node's lease in place of the local tmux check. A task on an offline node waits for it (see Progress records). A worker shutdown touches only that worker's sessions and never labels.

## Leading and hosting are separate

The leader needs the replicated state, the GitHub token, the router config, and one agent CLI for LLM routing. Without an agent CLI, routing falls back to `router.fallback_executor`. It needs no checkouts and no clones. Running an agent needs a checkout of the project and its toolchain.

A node with no projects is a coordinator. It votes, and it can lead if it has the token. The witness is a coordinator that never leads (see Roles).

So the machine with the most projects is the main worker, and it does not need to lead. A machine that sleeps can host every project and still be a learner. While it sleeps, the leader keeps ingesting and routing for all projects, and only tasks for projects that machine alone hosts wait.

## Nodes with different projects

The global `config.yml` lists local checkouts. Each project's settings are in its committed `.orch.yml`.

- On start and on config reload, each node writes `{node, projects, agents, max_parallel}` to the node registry. An offline node keeps its entry, marked offline, so its projects stay in the cluster.
- The cluster's projects are the union of those lists. The leader polls only those.
- The leader reads `.orch.yml` from the default branch through the GitHub contents API, with an ETag. A 304 response does not count against the rate limit. Every leader decides from the same commit, and a coordinator needs no clone. Rejected: bare clones in `~/.orch/projects/` on every node that can lead, which add a fetch loop per project on coordinators.
- A task goes only to a node that has its repo, has the agent the router picked, and has a free slot. The eligible agents are an input to routing (`src/engine/router/`), so the router does not pick an agent no host can run.
- When no eligible node is online, the task stays `routed` and shows "waiting for a node with owner/repo". It does not burn attempts.
- Leader work that needs a working tree, like the auto-merge rebase and force push (`src/engine/auto_merge.rs:143`) and `bash` jobs, is assigned to an eligible node like an agent run.

A project hosted on one machine has no failover. `orch nodes` lists those projects. A later option is a per-node setting to clone any cluster project on demand, for nodes that have the toolchains.

## Progress records

The session, its logs and the worktree stay on the node that runs the task. Only a short progress record goes through Raft.

The prompt asks the agent to commit and push after each step and then call `orch progress`:

```
orch progress --done "parser handles quoted args" --left "wire into CLI, tests" --state "tests fail on empty input"
```

`orch progress` first runs `git add -A` and `git commit` in the worktree with the `--done` text as the message, so a step the agent forgot to commit is still committed. The sandbox can deny that commit or the push, so the CLI then hands the text to the local daemon over the localhost websocket. The daemon runs outside the sandbox, holds the git credentials, and:

1. Commits what is still uncommitted with `auto_commit` (`src/engine/runner/git_ops.rs:384`).
2. Pushes the task branch with `push_branch` (`:941`) when origin is behind.
3. Proposes `{task, node, ts, branch, commit, committed_by, pushed_by, done, left, state}` to Raft. `committed_by` is `agent`, `cli`, `orch` or `none`, and `pushed_by` is `agent`, `orch` or `none`. `none` comes with the git error. `commit` is the head after step 1.

Session end already runs the same two helpers (`src/engine/runner/response_handler.rs:328`, `:462`), so a step and a session end share one path. Each text field is capped at a few hundred characters. Raft keeps the last 5 records per task. `orch task show` and the channels read them from any node. When the session ends, the leader posts the last record as an issue comment through the outbox.

A task stays with the node that started it:

- The node renews a lease while it is online. When the lease expires, the task shows "waiting for m4" and is not reassigned. It burns no attempts.
- When the node comes back, it resumes its own tasks from their worktrees, as a restart does today.
- `orch task move <id> [node]` hands a task to another node. The new node starts from the task branch as last pushed, and the new agent gets the progress records as context. Because the daemon pushes after each step, only work since the last `orch progress` call stays on the old node.

Rejected: snapshot commits of the worktree pushed to `refs/orch/checkpoints/<task>` so another node can take over automatically. They copy code on a timer, need a temporary index to keep the agent's index untouched, and move work off a machine that will be back in a few hours. Automatic takeover after a long hold can come later if a node goes away for good.

## Roles and deployment sizes

Raft needs a majority of voters up, so voters are machines that stay on.

| Role | Runs agents | Needs |
|---|---|---|
| Voter | yes | always on |
| Witness voter | no | always on, a few MB for the coordination state and the log, little CPU. A small VPS works |
| Learner | yes | nothing extra. It receives the full state and can go offline without affecting the majority |

Machines that sleep, like laptops, join as learners. A witness runs with openraft's `enable_elect: false`. It votes but never starts an election, so it never leads and needs no GitHub token, router config or agent CLI. openraft 0.9.25 has no `transfer_leader`, which exists only in the 0.10 alphas. `enable_elect` removes the case where a witness stays leader.

| Machines | Setup | Failover |
|---|---|---|
| 1 | one voter, the default | none, same as today |
| 2 | two voters plus a witness, or one voter and one learner | with the witness, either machine can fail. Without it, the learner adds capacity only |
| 3 or more always on | three voters, the rest learners | one voter can fail. Five voters survive two |

A fourth voter adds no fault tolerance, because four voters still survive only one failure. A voter that sleeps costs tolerance every time it sleeps, and forces an election if it was leader.

| Failure | Effect |
|---|---|
| A learner goes offline | Its tasks wait for it and resume from its worktrees when it is back. New tasks go to other nodes that host the project |
| One voter down | The others keep the majority |
| A network partition splits the voters | Only the side with a majority commits. The other side stops deciding |
| No majority | No new decisions. Running agents continue and completions retry |

Commit latency is one round trip from the leader to the nearest voter. On a LAN that is a few milliseconds, well under a GitHub API call.

## Transport and discovery

Nodes find each other on the tailnet. The operator lists no addresses.

| Piece | How |
|---|---|
| Node name | The first label of `DNSName` from `tailscale status --json`, for example `m1-max`. `HostName` can hold spaces |
| Node id | A hash of the node name, so the Raft id survives a reinstall of orch |
| Listen address | The node's own Tailscale IPv4 on `cluster.port`. Never `0.0.0.0` |
| Peers | Every 30 s, online peers from `tailscale status --json` get `GET /cluster/hello` with a 1 s timeout. Peers without orch time out, and peers with a different secret answer 401. Both are ignored |
| Auth | Every request carries `cluster.secret`. Tailscale encrypts the link. A tailnet ACL can restrict `cluster.port` to the cluster nodes |
| RPC | JSON over HTTP with axum and reqwest, as in `../agents-coordination` (`src/node.rs`). A follower forwards writes to the leader |

`/cluster/hello` returns `{name, id, role, leader, term, projects}`. `tailscale` is called as a CLI on macOS and Linux. Rejected: requiring a `tag:orch` ACL tag, because tagging m1 and m4 moves them from user-owned to tag-owned devices.

Membership:

- Voters are fixed by name in config, the same list on every voter: `cluster.voters`. When a voter has found an address for every name, it calls `initialize` with that membership. openraft accepts the same `initialize` from every voter, so no voter is special. Rejected: voters found by discovery, where two voters that start apart can each initialize a cluster of their own.
- Every other node with the secret is a learner. It sends `POST /cluster/join` to any peer, the peer forwards it to the leader, and the leader calls `add_learner`. An offline learner stays in the membership.
- A node with `cluster.witness: true` runs with `enable_elect: false` and runs no agents.
- With no `cluster` section, orch runs a one-node cluster with no listener and no call to `tailscale`. It behaves like orch today.

```yaml
cluster:
  secret: "${ORCH_CLUSTER_SECRET}"
  port: 7420
  voters: [m1-max, legion, vpsbr-16153223]
  witness: false   # true on vpsbr only
```

## Deployment on the current tailnet

| Node | OS | Role | Projects | Why |
|---|---|---|---|---|
| `m1-max` | macOS | voter | some | always on |
| `legion` | Linux, systemd | voter | some | always on |
| `vpsbr-16153223` | Linux, systemd | witness voter | none | always on, the third vote. Never leads, so it needs no token or agent CLI |
| `m4` | macOS | learner | all | almost always on, sleeps |

Any one of the three voters can fail and the cluster keeps deciding. m1 and legion trade the leader between them. While m4 sleeps, the leader keeps ingesting and routing. Tasks for projects only m4 hosts wait for it, and tasks for projects also on m1 or legion run there. Projects that need failover also need a checkout on m1 or legion.

Rejected: m4 as the third voter. When m4 sleeps and m1 or legion fails, no majority is left. A fourth voter adds no fault tolerance.

## Single-instance guards

#3729 adds two guards that hold on one machine and in a cluster: `push_branch` and the auto-merge rebase push pass an expected SHA to `--force-with-lease`, and `merge_pr` sends the head SHA. It is on hold for a regression: after orch's own `update_pr_branch` call in `poll_and_merge_recovered_pr` (`src/engine/sync.rs`), the recorded lease is stale and the next push fails. The review on #3729 lists the fix.

## PR order

1. Fix the push lease regression in #3729 and merge it.
2. Vetting PR for `openraft` 0.9. axum, reqwest and sqlx are already dependencies.
3. CLI writes go through the local daemon.
4. Cluster store: Raft commands for the replicated tables, one-node cluster as the default.
5. Leader-only decide phases, fencing, and the GitHub outbox.
6. Node registry, project-aware assignment, worker leases, recovery from the owner's lease, `orch task move`, and shutdown of own sessions only.
7. Listener on the tailnet address, cluster secret, tailnet discovery, voter bootstrap, learner join, `orch nodes`, and `orch stream` reading output from the node that runs the task.
8. `orch progress` and progress records.

## To verify before building

- Whether `tailscale status --json` reports the same `DNSName` on macOS (the App Store build and the standalone build) and on Linux.

## Costs

- Step 4 is the large one.
- Apply must be deterministic, which rules out clock reads and random values inside it.
- Failover pauses decisions for about one election timeout. Running agents continue.
- `.orch.yml` changes reach the cluster after they merge to the default branch.
- A project on one machine waits while that machine is offline.
- The daemon pushes after each step, so a repo whose CI runs on every push starts one CI run per step. A workflow with `cancel-in-progress` keeps only the newest.
- Work on an offline node waits for that node, and work it never pushed is reachable only there.
- A cluster needs Tailscale on every node. Discovery reads the `tailscale` CLI output, which Tailscale does not promise to keep stable.
- Adding or removing a voter is a config change on every voter.
- The listener changes the "No external endpoints" rule (`CLAUDE.md:372-374`, `AGENTS.md`), and the rule text has to change with it.
