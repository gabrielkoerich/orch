fn main() {
    let e = format!("GitHub API DELETE https://api.github.com/repos/gabrielkoerich/bean/git/refs/heads/internal-52655-market-intelligence-trending-topics-stoc failed (422 Unprocessable Entity): {{\"message\":\"Reference does not exist\",\"documentation_url\":\"https://docs.github.com/rest/git/refs#delete-a-reference\",\"status\":\"422\"}}");
    let err_str = e.to_string();
    println!("Contains: {}", err_str.contains("Reference does not exist"));
    println!("Err: {}", err_str);
}
