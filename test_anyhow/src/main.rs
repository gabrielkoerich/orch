use anyhow::bail;
use tracing_subscriber;

fn do_err() -> anyhow::Result<()> {
    let e = std::io::Error::new(std::io::ErrorKind::Other, "underlying error body: {\"message\":\"Reference does not exist\"}");
    bail!("GitHub API DELETE failed: {}", e);
}

fn main() {
    tracing_subscriber::fmt::init();
    if let Err(e) = do_err() {
        let err_str = e.to_string();
        println!("Contains: {}", err_str.contains("Reference does not exist"));
        println!("Err to_string: {}", err_str);
        tracing::warn!(err = %e, "This is the warning");
    }
}
