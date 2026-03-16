Panic / unwrap / expect Audit
=============================

What I did
- Scanned the source tree for `unwrap()`, `expect()`, and `panic!()` occurrences in runtime code (excluded few pure test-only files when obvious).
- Created this short actionable report mapping high-priority hotspots to recommended fixes.

High-priority hotspots (start here)
- `src/tmux.rs` — found uses that quietly swallow errors (e.g. `capture_pane(...).await.unwrap_or_default()` in `wait_for_completion`) and several `unwrap()` calls inside `#[cfg(test)]` tests. Runtime helpers like `wait_for_completion`, `snapshot` and `session_exists` should propagate errors (return `Result`) or log & retry instead of swallowing.

- `src/store.rs` — heavy use of `.await.unwrap()` across many integration/unit tests and some helper code (e.g. `TaskStore::open_memory().await.unwrap()`). Tests should keep `unwrap()` when asserting happy paths; runtime helpers that open stores or perform updates must return `Result` and bubble errors for the caller to handle.

- `src/template.rs` — tests use `unwrap()` extensively; the library API already returns `Result<String, String>` which is good. Replace test `unwrap()`s in favor of explicit assertions where non-happy path behavior is needed; consider changing `render_template_with_vars` error type to `anyhow::Error` or a typed error enum for richer context.

- `src/security.rs` — originally many `expect()` calls were reported; current code logs regex compilation failures and skips invalid patterns (good). No panics in production code here; tests use assertions as usual. Keep the current non-panicking behavior.

- `src/channels/transport.rs` — contains `panic!()` in `#[cfg(test)]` assertions (e.g. `panic!("expected TaskSession")`) and runtime pattern matching that used to panic in earlier versions. Ensure runtime routing code never `panic!()` on malformed input — return an `Err` or an explicit `MessageRoute::NewTask` fallback.

- `src/engine/tasks.rs` & `src/engine/runner/response.rs` — a handful of `panic!()` calls used as defensive checks (e.g. `panic!("expected Internal variant")`, `panic!("expected MissingTool error")`). Replace with `unreachable!()` only if provably unreachable, or better return a `Result`/`Error` and propagate.

Quantitative summary (sample)
- Total matches found across repo for `unwrap`/`expect`/`panic!`: 982 (includes tests).  Focus first on the 20–30 occurrences in runtime-critical modules named above.

Recommended remediation plan (short)
1) Immediate audit pass (done): catalog hotspots (this file).
2) For each runtime function that currently swallows or panics on errors:
   - Change signature to return `anyhow::Result<T>` or a repo-consistent error type.
   - Use `?` to propagate errors or `tracing::warn!`/`error!` + retry/fallback where appropriate.
   - Add contextual tracing fields (task_id, session, repo) when logging.
3) For defensive `panic!()` in code paths that truly are unreachable, add a clear comment and consider `unreachable!()`; prefer returning `Result` instead where external input could cause the condition.
4) Tests: keep `unwrap()` in tests that assert happy paths; add new tests that exercise error paths (mock failures) to ensure we don't regress.
5) Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo nextest run` and iterate until green.

Suggested first concrete PRs (small incremental changes)
1. `src/tmux.rs`: Change `wait_for_completion` to return `Result<String>` and propagate errors from `capture_pane` instead of `unwrap_or_default()`; update callers.
2. `src/channels/transport.rs`: Replace test `panic!()` branches with `assert_matches` or explicit pattern match arms returning `MessageRoute::NewTask`; make `route` fully non-panicking.
3. `src/engine/runner/response.rs`: Replace `panic!()` assertions with typed errors in parsing code paths and add unit tests for the error cases.

Next steps I can take now
- Implement PR #1 (`src/tmux.rs` change) and add unit tests that simulate tmux command failures (use `Command` wrapper mocking where available).
- After PR #1 is merged, iterate on PR #2 and PR #3.

Location of this report
- `reports/panic_audit.md` (this file)

If you want, I will proceed to implement the `tmux.rs` change first (small, low-risk) and push a branch with tests.
