fn main() {
    let body = r#"{"message":"Reference does not exist","documentation_url":"..."}"#;
    let e = anyhow::anyhow!("GitHub API DELETE url failed (422): {}", body);
    let err_str = e.to_string();
    println!("Contains: {}", err_str.contains("Reference does not exist"));
    println!("Err: {}", err_str);
}
