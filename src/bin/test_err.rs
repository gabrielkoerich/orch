use anyhow::Context;
fn main() {
    let e = anyhow::anyhow!("GitHub API DELETE ... failed (422 Unprocessable Entity): {{\"message\":\"Reference does not exist\",\"documentation_url\":\"https://docs.github.com/rest/git/refs#delete-a-reference\",\"status\":\"422\"}}");
    println!("e.to_string() = {}", e.to_string());
    println!("format!(\"{{e}}\") = {}", format!("{e}"));
    println!(
        "contains = {}",
        e.to_string().contains("Reference does not exist")
    );
}
