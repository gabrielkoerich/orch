/**
 * Model names live in the config, never in code. The test takes the model names from
 * `config.example.yml` and fails on any string literal in `src/` that holds one, outside
 * `#[cfg(test)]` items and test files
 **/
use regex::Regex;
use serde_norway::Value;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;

const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
const EXAMPLE_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/config.example.yml");

static RAW_OPENER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:^|[^A-Za-z0-9_])r(#+)""#).expect("raw string regex"));
static LITERAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"r#*"([^"]*)"|"((?:[^"\\]|\\.)*)""#).expect("literal regex"));

/// Model names under `models:` and `model_map:` in the example config. A pool keyword such as
/// `opencode:free` names no model and is left out
fn configured_model_names() -> Vec<String> {
    let text = fs::read_to_string(EXAMPLE_CONFIG).expect("read config.example.yml");
    let config: Value = serde_norway::from_str(&text).expect("parse config.example.yml");
    let mut names = Vec::new();
    collect_strings(config.get("models"), &mut names);
    collect_strings(config.get("model_map"), &mut names);
    names.retain(|n| !n.is_empty() && !n.contains(':'));
    names.sort();
    names.dedup();
    names
}

fn collect_strings(value: Option<&Value>, out: &mut Vec<String>) {
    match value {
        Some(Value::String(s)) => out.push(s.to_lowercase()),
        Some(Value::Sequence(items)) => {
            for item in items {
                collect_strings(Some(item), out);
            }
        }
        Some(Value::Mapping(map)) => {
            for item in map.values() {
                collect_strings(Some(item), out);
            }
        }
        _ => {}
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn is_test_file(path: &Path) -> bool {
    let stem = path.file_stem().and_then(|n| n.to_str()).unwrap_or("");
    stem == "tests"
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || path.components().any(|c| c.as_os_str() == "tests")
}

/// Lines outside `#[cfg(test)]` items, with 1-based line numbers. rustfmt closes an item with
/// a `}` at the indent of its attribute, so that line ends the item
fn non_test_lines(source: &str) -> Vec<(usize, &str)> {
    let mut kept = Vec::new();
    let mut lines = source.lines().enumerate();
    while let Some((idx, line)) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            let indent = &line[..line.len() - trimmed.len()];
            let closing = format!("{indent}}}");
            let mut first_line = true;
            let mut raw_closer: Option<String> = None;
            for (_, item_line) in lines.by_ref() {
                let item = item_line.trim_end();
                // A `}` inside a multi-line raw string does not close the item
                if let Some(closer) = &raw_closer {
                    if item.contains(closer.as_str()) {
                        raw_closer = None;
                    }
                    continue;
                }
                raw_closer = open_raw_string(item);
                // Attributes stacked under `#[cfg(test)]` belong to the same item
                let attribute = item.trim_start().starts_with("#[") && item.ends_with(']');
                if first_line && !attribute && item.ends_with(';') {
                    break;
                }
                if item == closing {
                    break;
                }
                first_line &= attribute;
            }
            continue;
        }
        kept.push((idx + 1, line));
    }
    kept
}

/// The closer of a `r#"` raw string the line opens and leaves open
fn open_raw_string(line: &str) -> Option<String> {
    let mut from = 0;
    while let Some(caps) = RAW_OPENER.captures_at(line, from) {
        let closer = format!("\"{}", &caps[1]);
        let body = caps.get(0).expect("whole match").end();
        match line[body..].find(&closer) {
            Some(at) => from = body + at + closer.len(),
            None => return Some(closer),
        }
    }
    None
}

/// String literals on a line of code, comments excluded
fn string_literals(line: &str) -> Vec<String> {
    let code = line.trim_start();
    if code.starts_with("//") {
        return Vec::new();
    }
    LITERAL
        .captures_iter(code)
        .filter_map(|c| c.get(1).or_else(|| c.get(2)))
        .map(|m| m.as_str().to_lowercase())
        .collect()
}

fn names_model(literal: &str, name: &str) -> bool {
    let boundary = |c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'));
    literal.match_indices(name).any(|(start, _)| {
        let before = literal[..start].chars().next_back();
        let after = literal[start + name.len()..].chars().next();
        before.is_none_or(boundary) && after.is_none_or(boundary)
    })
}

#[test]
fn source_names_no_models() {
    let names = configured_model_names();
    assert!(!names.is_empty(), "config.example.yml lists no models");

    let mut files = Vec::new();
    rust_files(Path::new(SRC), &mut files);
    files.sort();

    let mut hits = Vec::new();
    for path in files.iter().filter(|p| !is_test_file(p)) {
        let source = fs::read_to_string(path).expect("read source file");
        for (line_no, line) in non_test_lines(&source) {
            for literal in string_literals(line) {
                if let Some(name) = names.iter().find(|n| names_model(&literal, n)) {
                    let rel = path
                        .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                        .unwrap_or(path);
                    hits.push(format!(
                        "{}:{line_no}: \"{literal}\" names model {name}",
                        rel.display()
                    ));
                }
            }
        }
    }

    assert!(
        hits.is_empty(),
        "model names belong in the config, not in code:\n{}",
        hits.join("\n")
    );
}

#[test]
fn names_model_matches_whole_names_only() {
    assert!(names_model("sonnet", "sonnet"));
    assert!(names_model("--model sonnet", "sonnet"));
    assert!(!names_model("sonnets", "sonnet"));
    assert!(!names_model("claude-sonnet-4", "sonnet"));
}

#[test]
fn non_test_lines_skips_cfg_test_items() {
    let source = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() {}\n}\nfn c() {}\n";
    let kept: Vec<usize> = non_test_lines(source).into_iter().map(|(n, _)| n).collect();
    assert_eq!(kept, vec![1, 6]);
}

#[test]
fn non_test_lines_skips_a_cfg_test_mod_declaration() {
    let source = "#[cfg(test)]\nmod tests;\nfn c() {}\n";
    let kept: Vec<usize> = non_test_lines(source).into_iter().map(|(n, _)| n).collect();
    assert_eq!(kept, vec![3]);
}
