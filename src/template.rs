use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::sync::LazyLock;

/// Matches `{{#if VAR}}...{{/if}}` blocks (non-greedy, dotall).
fn compile_pattern(pattern: &str, label: &str) -> Result<Regex, String> {
    Regex::new(pattern).map_err(|err| format!("invalid {label} regex: {err}"))
}

fn if_pattern() -> Result<&'static Regex, String> {
    static IF_PATTERN: LazyLock<Result<Regex, String>> =
        LazyLock::new(|| compile_pattern(r"(?s)\{\{#if\s+(\w+)\}\}(.*?)\{\{/if\}\}", "if_pattern"));
    IF_PATTERN.as_ref().map_err(|err| err.clone())
}

/// Matches `{{VAR}}` variable placeholders.
fn var_pattern() -> Result<&'static Regex, String> {
    static VAR_PATTERN: LazyLock<Result<Regex, String>> =
        LazyLock::new(|| compile_pattern(r"\{\{(\w+)\}\}", "var_pattern"));
    VAR_PATTERN.as_ref().map_err(|err| err.clone())
}

fn render_template_with_vars(
    template: &str,
    vars: &HashMap<String, String>,
) -> Result<String, String> {
    let mut data = template.to_string();

    loop {
        let mut changed = false;
        let new_data = if_pattern()?
            .replace_all(&data, |caps: &regex::Captures| {
                changed = true;
                let var_name = &caps[1];
                let content = &caps[2];
                match vars.get(var_name) {
                    Some(value) if !value.trim().is_empty() => content.to_string(),
                    _ => String::new(),
                }
            })
            .to_string();

        data = new_data;
        if !changed {
            break;
        }
    }

    let result = var_pattern()?
        .replace_all(&data, |caps: &regex::Captures| {
            let var_name = &caps[1];
            vars.get(var_name).cloned().unwrap_or_default()
        })
        .to_string();

    if result.is_empty() {
        return Err("empty output".to_string());
    }

    Ok(result)
}

pub fn render_template_str(
    template: &str,
    vars: &HashMap<String, String>,
) -> Result<String, String> {
    render_template_with_vars(template, vars)
}

pub fn render_template(template_path: &str, extra_vars: &[String]) -> Result<String, String> {
    if !fs::metadata(template_path)
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return Err(format!("template not found: {}", template_path));
    }

    let data =
        fs::read_to_string(template_path).map_err(|e| format!("failed to read template: {}", e))?;

    let mut vars: HashMap<String, String> = HashMap::new();

    for var in extra_vars {
        if let Some((key, value)) = var.split_once('=') {
            vars.insert(key.to_string(), value.to_string());
        }
    }

    render_template_with_vars(&data, &vars)
}

pub fn render_and_print(template_path: &str, extra_vars: &[String]) -> io::Result<()> {
    match render_template(template_path, extra_vars) {
        Ok(output) => {
            io::stdout().write_all(output.as_bytes())?;
            Ok(())
        }
        Err(e) => {
            io::stderr().write_all(e.as_bytes())?;
            Err(io::Error::other(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn render_template_does_not_leak_env_vars() {
        // Set a sensitive env var that must NOT appear in the rendered output.
        // temp_env provides RAII-scoped environment changes so tests remain hermetic.
        temp_env::with_var("ORCH_TEST_SECRET_TOKEN", Some("should-not-appear"), || {
            let mut f = NamedTempFile::new().unwrap();
            writeln!(f, "hello world").unwrap();

            let result = render_template(f.path().to_str().unwrap(), &[]).unwrap();
            assert!(
                !result.contains("should-not-appear"),
                "env var leaked into rendered template"
            );
        });
    }

    #[test]
    fn render_template_uses_explicit_vars() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "value={{{{MY_VAR}}}}").unwrap();

        let result =
            render_template(f.path().to_str().unwrap(), &["MY_VAR=hello".to_string()]).unwrap();
        assert_eq!(result.trim(), "value=hello");
    }

    #[test]
    fn compile_pattern_rejects_invalid_regex() {
        let err = compile_pattern("(", "bad_pattern").unwrap_err();
        assert!(err.contains("bad_pattern"));
    }
}
