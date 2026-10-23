# Strict named-profile routing: offline native proof

This is experimental groundwork for an opt-in named-persona policy. It is **not a deployable worker mode**. The service refuses to start with the experimental option set, and strict task execution, direct calls, review and fallback all fail closed. `worker_execution_enabled: true` is rejected. Do not enable this option on a running installation.

## Scope and review acceptance

This change is independently reviewable as an **offline foundation**: immutable executor/reviewer assignments, native argument construction, local transactional persistence, revocation checks and unconditional process guards. Review acceptance covers those behaviors and compatibility with ordinary unpinned routing. It does not establish live worker safety or enable execution.

The complete named-profile feature remains open in [the original request](https://github.com/gabrielkoerich/orch/issues/3720). [Runtime acceptance before enabling workers](https://github.com/gabrielkoerich/orch/issues/3730) tracks the remaining identity, spawn authorization, capability, fenced lease, reset and recovery work. Its shared-authority decision must align with [the existing cluster decision](https://github.com/gabrielkoerich/orch/issues/3728); this proof introduces no transport, service or new dependency to make that choice implicitly.

Passing the offline checks or marking this patch ready for review leaves every strict execution guard in place. Runtime enablement requires a separate implementation and acceptance of the follow-up issue.

A persona such as `agent:designer` identifies a complete route: worker host, CLI, source provider, explicit CLI-provider binding, model and reasoning effort. The profile must be enabled, current, allow the repository, and disable fallback. Admission requires an open task with exactly `execution:ready`; `execution:held` always wins. Control label namespaces and revocations are matched without ASCII case sensitivity, as GitHub label names are; persona identifiers retain their configured spelling. `no-agent`, done and archived labels block admission. Unknown policy fields are rejected instead of ignoring security-setting typos. Unknown personas, ambiguous labels, missing dimensions, unavailable capabilities and aliases absent from the supplied capability map are rejected.

The capability map is an operator-supplied input in this proof. It does not authenticate a host, discover live CLI/account capabilities, or establish that a provider accepts a particular model or effort. No model inference is performed by the tests.

## Native integration

`ORCH_STRICT_PROFILE_REGISTRY` points to the experimental JSON policy and `ORCH_STRICT_WORKER_HOST` identifies the worker. Both are required for strict admission. When unset, existing route selection and command construction remain available.

`Router::route` checks strict policy before consulting the generic stored-agent, label/complexity, LLM and round-robin paths. A conflicting stored agent/model is rejected. `Router::store_route_result` records the complete profile in an immutable SQLite KV entry using the existing atomic insert-if-absent primitive. The global routing sweep also persists this pin. Subsequent route retrieval compares the pin with current policy, labels, selected CLI and selected model; it does not discard a pinned model because it is outside the generic complexity pool. The pin and native route fields are now committed in one SQLite transaction. Both the router and the commit recheck cached revocations without stripping workflow labels. A failed route write rolls back a newly inserted pin. A pin continues to block legacy fallback even if the opt-in environment variable is later removed. Both native dispatch sweeps persist Blocked through TaskManager, preserving its best-effort backend status mirror and status event, before InProgress, capture or tmux; the runner also returns an explicit Blocked signal instead of sending a proof-only rejection through generic error-to-review handling. Registry byte changes require explicit future operator reconciliation; this proof never silently replaces a pin.

Task initialization checks the persisted pin and requires a successful fresh backend task read before creating a worktree. Closed, held, archived, reassigned or mismatched issues and backend failures block preparation. This read is not a spawn authorization ticket: it does not authenticate the worker or prevent a revocation between preparation and spawn, and strict spawning is still unconditionally disabled. Missing caller agent/model cannot fall back to a default CLI. Error recovery and generic failover atomically persist Blocked and its reason using the native status API (rather than trying to write status through the field allowlist), instead of selecting a cheaper model, another CLI or another provider. Persona labels are preserved instead of being rewritten into backend labels.

The real runner implementations have a pure `build_profile_command` method:

- Codex: pinned `--model`, `-c model_provider=...` and `-c model_reasoning_effort=...`, with config overrides after `exec`.
- Claude Code: pinned `--model` and `--effort`; the native Anthropic binding is required. Claude-compatible wrappers are not treated as Claude Code.
- OpenCode: explicit `provider/model` and `--variant`.

Legacy `build_command` calls use the same inner implementation without the additional profile flags. Unsupported backends reject profile command construction. A command string alone is not execution authorization.

## Independent reviewer assignment

An optional, explicit `reviewer:<persona>` label selects a reviewer from the same policy. Its host/CLI/provider/model/effort and source identity are validated and stored as part of the original immutable assignment. Missing or unknown reviewer choices never select a default. Multiple reviewer labels, the executor's persona, or another persona with the same source identity are rejected. Two independent identities may use the same CLI; changing backend alone does not establish independence.

The native review entry point validates this persisted assignment before recording a review session or entering generic reviewer selection. It materializes a separate reviewer invocation pin with the reviewer's own dimensions. The invocation still hits the unconditional execution guard; no reviewer process runs. Adding, removing or changing a reviewer label after pinning is policy drift and requires reconciliation, rather than silently changing the reviewer.

## Restart and reset boundary

File-backed tests close and reopen SQLite, retrieve both pins, and reject changed registry bytes or missing policy. Database fault injection and independent connections exercise rollback and conflicting route commits. These are local storage guarantees, not a distributed lease or a live daemon restart proof.

Native `internal:<row>` aliases resolve through the repository-scoped task resolver before a missing pin can permit ordinary routing. Looking up an external issue through this alias retains its immutable assignment, cached revocations and execution/fallback guards, including after policy removal. Conflicting alias records or pins bound to another key fail closed. This handles alternate identifiers for the same stored task; it does not authorize rebinding a task to a different external issue.

Alias support is limited to pin reads and guards. New strict route admission and route persistence require the canonical task identifier; supplying an alias there is rejected rather than selecting another route.

There is deliberately no pin deletion or reset command. Retrying a task preserves its executor and reviewer. A safe reset needs an authenticated operator decision against an expected old pin, a durable audit record, and proof that the old execution lease is fenced and its worker has stopped. Neither a local status change nor clearing retry counters proves that a remote worker stopped. Without a shared fencing authority, deleting/replacing a pin could allow a previous worker and a new assignment to overlap.

## Offline verification

Run `cargo test --lib strict_profiles -- --test-threads=1`. The tests use synthetic profiles and in-memory SQLite. Native command strings run against inert executables in a temporary directory with a cleared environment. Those executables record arguments; they never contact a provider. Other tests exercise the actual native router, persistence, task initialization, error recovery and process guards.

The service entry-point test reaches only the new first-statement rejection; it does not start the engine or inspect/stop existing services. The direct-command test uses a temporary-file sentinel and proves the command never starts.

## Unfinished execution contract

Before live workers could be enabled, this design still needs authenticated host binding, fresh remote authorization immediately before spawn, cross-host durable claims/leases with fencing and cancellation, a controlled pin-reset workflow, CLI/account capability validation, and complete live restart/retry/handoff reconciliation. Reviewer selection/persistence and preparation-time backend revalidation are now implemented, but actual reviewer execution and spawn-time authorization remain disabled. The current local KV compare-and-set is immutable route storage, not a distributed execution lease. The provided worker-host string is not a security boundary.

The proof deliberately blocks all strict execution while these parts are unresolved. It supplies no scheduler activation, live GitHub queue consumer, remote host launcher, paid inference test or automatic policy enabling.

The current architecture supplies a local SQLite store and a caller-provided host string. It supplies no authenticated remote-worker enrollment, shared lease authority, or authenticated CLI/account capability attestation protocol. Choosing those trust boundaries requires an operator/maintainer integration decision; a successful CLI discovery or an inert argument recorder cannot establish them. No externally reachable endpoint is introduced by this proof.
