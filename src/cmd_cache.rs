use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;

static CMD_CACHE: Lazy<Mutex<HashMap<String, bool>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Check if a command exists in PATH, caching the result to avoid repeated
/// filesystem scans (which can be expensive when called frequently).
pub fn command_exists(cmd: &str) -> bool {
    let mut cache = CMD_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = cache.get(cmd) {
        return *v;
    }
    let ok = which::which(cmd).is_ok();
    cache.insert(cmd.to_string(), ok);
    ok
}
