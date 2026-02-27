use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::sync::LazyLock;

/// Matches `{{#if VAR}}...{{/if}}` blocks (non-greedy, dotall).
static IF_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)\{\{#if\s+(\w+)\}\}(.*?)\{\{/if\}\}")
        .expect("BUG: if_pattern regex is invalid")
});

/// Matches `{{VAR}}` variable placeholders.
static VAR_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{\{(\w+)\}\}").expect("BUG: var_pattern regex is invalid")
});

pub fn render_template(template_path: &str, extra_vars: &[String]) -> Result<String, String> {
    if !fs::metadata(template_path)
        .map(|m| m.is_file())
        .unwrap_or(false)
    {
        return Err(format!("template not found: {}", template_path));
    }

    let mut data =
        fs::read_to_string(template_path).map_err(|e| format!("failed to read template: {}", e))?;

    let mut vars: HashMap<String, String> = env::vars().collect();

    for var in extra_vars {
        if let Some((key, value)) = var.split_once('=') {
            vars.insert(key.to_string(), value.to_string());
        }
    }

    loop {
        let mut changed = false;
        let new_data = IF_PATTERN
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

    let result = VAR_PATTERN
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
