use std::collections::HashMap;
use std::sync::Mutex;

static CMD_CACHE: std::sync::LazyLock<Mutex<HashMap<String, bool>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Check if a command exists in PATH, caching the result to avoid repeated
/// filesystem scans (which can be expensive when called frequently).
pub fn command_exists(cmd: &str) -> bool {
    let mut cache = CMD_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(v) = cache.get(cmd) {
        return *v;
    }
    let ok = which::which(cmd).is_ok();
    cache.insert(cmd.to_string(), ok);
    ok
}

#[cfg(test)]
pub fn reset_for_test() {
    CMD_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}
