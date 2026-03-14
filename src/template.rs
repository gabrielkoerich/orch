use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::sync::LazyLock;

/// Matches `{{#if VAR}}...{{/if}}` blocks (non-greedy, dotall).
static IF_PATTERN: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"(?s)\{\{#if\s+(\w+)\}\}(.*?)\{\{/if\}\}"));

/// Matches `{{VAR}}` variable placeholders.
static VAR_PATTERN: LazyLock<Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"\{\{(\w+)\}\}"));

fn render_template_with_vars(
    template: &str,
    vars: &HashMap<String, String>,
) -> Result<String, String> {
    let if_pattern = IF_PATTERN
        .as_ref()
        .map_err(|e| format!("invalid if template regex: {e}"))?;
    let var_pattern = VAR_PATTERN
        .as_ref()
        .map_err(|e| format!("invalid variable template regex: {e}"))?;
    let mut data = template.to_string();

    loop {
        let mut changed = false;
        let new_data = if_pattern
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

    let result = var_pattern
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
    fn render_template_does_not_leak_env_vars() -> anyhow::Result<()> {
        // Set a sensitive env var that must NOT appear in the rendered output
        unsafe {
            std::env::set_var("ORCH_TEST_SECRET_TOKEN", "should-not-appear");
        }

        let mut f = NamedTempFile::new()?;
        writeln!(f, "hello world")?;

        let path = f
            .path()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("temp path is not valid UTF-8"))?;
        let result = render_template(path, &[]).map_err(|e| anyhow::anyhow!(e))?;
        assert!(
            !result.contains("should-not-appear"),
            "env var leaked into rendered template"
        );

        unsafe {
            std::env::remove_var("ORCH_TEST_SECRET_TOKEN");
        }
        Ok(())
    }

    #[test]
    fn render_template_uses_explicit_vars() -> anyhow::Result<()> {
        let mut f = NamedTempFile::new()?;
        writeln!(f, "value={{{{MY_VAR}}}}")?;

        let path = f
            .path()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("temp path is not valid UTF-8"))?;
        let result =
            render_template(path, &["MY_VAR=hello".to_string()]).map_err(|e| anyhow::anyhow!(e))?;
        assert_eq!(result.trim(), "value=hello");
        Ok(())
    }
}
