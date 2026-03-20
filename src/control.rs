// src/control.rs - stub, will be replaced
pub fn agent_for_model(_model: &str) -> &'static str {
    "claude"
}

pub async fn get_model(_store: &crate::store::TaskStore) -> String {
    "sonnet".to_string()
}

pub async fn send_message(
    _store: &crate::store::TaskStore,
    _channel: &str,
    _thread: Option<&str>,
    _msg: &str,
) -> anyhow::Result<String> {
    Ok(String::new())
}
