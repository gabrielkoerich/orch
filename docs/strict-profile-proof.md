# Strict named-profile routing: offline native proof

This is experimental groundwork for an opt-in named-persona policy. It is **not a deployable worker mode**. The service refuses to start with the experimental option set, and strict task execution, direct calls, review and fallback all fail closed. `worker_execution_enabled: true` is rejected. Do not enable this option on a running installation.

A persona such as `agent:designer` identifies a complete route: worker host, CLI, source provider, explicit CLI-provider binding, model and reasoning effort. The profile must be enabled, current, allow the repository, and disable fallback. Admission requires an open task with exactly `execution:ready`; `execution:held` always wins. Unknown personas, ambiguous labels, missing dimensions, unavailable capabilities and aliases absent from the supplied capability map are rejected.

The capability map is an operator-supplied input in this proof. It does not authenticate a host, discover live CLI/account capabilities, or establish that a provider accepts a particular model or effort. No model inference is performed by the tests.

## Native integration

`ORCH_STRICT_PROFILE_REGISTRY` points to the experimental JSON policy and `ORCH_STRICT_WORKER_HOST` identifies the worker. Both are required for strict admission. When unset, existing route selection and command construction remain available.

`Router::route` checks strict policy before consulting the generic stored-agent, label/complexity, LLM and round-robin paths. A conflicting stored agent/model is rejected. `Router::store_route_result` records the complete profile in an immutable SQLite KV entry using the existing atomic insert-if-absent primitive. The global routing sweep also persists this pin. Subsequent route retrieval compares the pin with current policy, labels, selected CLI and selected model; it does not discard a pinned model because it is outside the generic complexity pool. A pin continues to block legacy fallback even if the opt-in environment variable is later removed. Registry byte changes require explicit future operator reconciliation; this proof never silently replaces a pin.

Task initialization checks the persisted pin before creating a worktree. Missing caller agent/model cannot fall back to a default CLI. Error recovery and generic failover block the task instead of selecting a cheaper model, another CLI or another provider. Persona labels are preserved instead of being rewritten into backend labels.

The real runner implementations have a pure `build_profile_command` method:

- Codex: pinned `--model`, `-c model_provider=...` and `-c model_reasoning_effort=...`, with config overrides after `exec`.
- Claude Code: pinned `--model` and `--effort`; the native Anthropic binding is required. Claude-compatible wrappers are not treated as Claude Code.
- OpenCode: explicit `provider/model` and `--variant`.

Legacy `build_command` calls use the same inner implementation without the additional profile flags. Unsupported backends reject profile command construction. A command string alone is not execution authorization.

## Offline verification

Run `cargo test --lib strict_profiles -- --test-threads=1`. The tests use synthetic profiles and in-memory SQLite. Native command strings run against inert executables in a temporary directory with a cleared environment. Those executables record arguments; they never contact a provider. Other tests exercise the actual native router, persistence, task initialization, error recovery and process guards.

The service entry-point test reaches only the new first-statement rejection; it does not start the engine or inspect/stop existing services. The direct-command test uses a temporary-file sentinel and proves the command never starts.

## Unfinished execution contract

Before live workers could be enabled, this design still needs authenticated host binding, fresh remote authorization immediately before spawn, cross-host durable claims/leases, a controlled pin-reset workflow, CLI/account capability validation, independently pinned reviewer profiles, and complete restart/retry/handoff reconciliation. The current local KV compare-and-set is immutable route storage, not a distributed execution lease. The provided worker-host string is not a security boundary.

The proof deliberately blocks all strict execution while these parts are unresolved. It supplies no scheduler activation, live GitHub queue consumer, remote host launcher, paid inference test or automatic policy enabling.
