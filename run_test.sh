sed -i '' -e 's/let _guard = ENV_LOCK.lock().unwrap();/let _guard = ENV_LOCK.lock().unwrap(); let _ = tracing_subscriber::fmt().with_env_filter("debug").try_init();/g' src/engine/runner/mod.rs
cargo test run_with_context_blocks_when_budget_exceeded_pre_run -- --nocapture
