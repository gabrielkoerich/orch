//! Edit the `projects:` list in `config.yml` as text so comments stay

use anyhow::Context;

/// Result of a comment-preserving `projects:` edit
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectsEdit {
    /// Path already present on add, or missing on remove
    Unchanged,
    /// New document text
    Updated(String),
}

/// Insert `path` into the root `projects:` list
pub fn add_project(content: &str, path: &str) -> anyhow::Result<ProjectsEdit> {
    match parsed_string_projects(content)? {
        ParsedProjects::NotAList => anyhow::bail!("projects is not a list"),
        ParsedProjects::List(items) if items.iter().any(|p| p == path) => {
            Ok(ProjectsEdit::Unchanged)
        }
        ParsedProjects::Missing | ParsedProjects::List(_) => insert_project(content, path),
    }
}

/// Remove list entries whose value equals any of `candidates`
pub fn remove_project(content: &str, candidates: &[&str]) -> anyhow::Result<ProjectsEdit> {
    match parsed_string_projects(content)? {
        ParsedProjects::Missing => anyhow::bail!("no projects list in config"),
        ParsedProjects::NotAList => anyhow::bail!("no projects list in config"),
        ParsedProjects::List(items) => {
            if !items.iter().any(|p| candidates.contains(&p.as_str())) {
                return Ok(ProjectsEdit::Unchanged);
            }
            delete_project(content, candidates)
        }
    }
}

enum ParsedProjects {
    Missing,
    NotAList,
    List(Vec<String>),
}

fn parsed_string_projects(content: &str) -> anyhow::Result<ParsedProjects> {
    if content.trim().is_empty() {
        return Ok(ParsedProjects::Missing);
    }
    let doc: serde_norway::Value = serde_norway::from_str(content).context("parsing config.yml")?;
    let Some(root) = doc.as_mapping() else {
        anyhow::bail!("config is not a YAML mapping");
    };
    match root.get("projects") {
        None | Some(serde_norway::Value::Null) => Ok(ParsedProjects::Missing),
        Some(serde_norway::Value::Sequence(seq)) => Ok(ParsedProjects::List(
            seq.iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect(),
        )),
        Some(_) => Ok(ParsedProjects::NotAList),
    }
}

fn insert_project(content: &str, path: &str) -> anyhow::Result<ProjectsEdit> {
    let (mut lines, newline_at_end) = split_lines(content);
    match find_root_projects(&lines) {
        None => {
            append_projects_block(&mut lines, path);
            Ok(ProjectsEdit::Updated(join_lines(&lines, newline_at_end)))
        }
        Some(key_line) => {
            let same_line = projects_value(&lines[key_line]).unwrap_or("").to_string();
            let (value, comment) = split_value_and_comment(&same_line);
            let value = value.trim();
            if is_complete_flow(value) && value != "[]" {
                let updated = add_to_flow_line(&lines[key_line], path)
                    .ok_or_else(|| anyhow::anyhow!("could not edit projects: list as text"))?;
                lines[key_line] = updated;
                return Ok(ProjectsEdit::Updated(join_lines(&lines, newline_at_end)));
            }
            if is_empty_list_value(value) {
                if !comment.trim().is_empty() {
                    lines[key_line] = format!("projects:{comment}");
                } else {
                    lines[key_line] = "projects:".to_string();
                }
            } else if !value.is_empty() {
                anyhow::bail!("could not edit projects: list as text");
            }
            let indent = detect_indent(&lines, key_line);
            let insert_at = insert_index(&lines, key_line) + 1;
            lines.insert(insert_at, format!("{indent}- {}", format_yaml_string(path)));
            Ok(ProjectsEdit::Updated(join_lines(&lines, newline_at_end)))
        }
    }
}

fn delete_project(content: &str, candidates: &[&str]) -> anyhow::Result<ProjectsEdit> {
    let (mut lines, newline_at_end) = split_lines(content);
    let key_line = find_root_projects(&lines)
        .ok_or_else(|| anyhow::anyhow!("could not edit projects: list as text"))?;
    let same_line = projects_value(&lines[key_line]).unwrap_or("").to_string();
    let (value, _) = split_value_and_comment(&same_line);
    let value = value.trim();
    if is_complete_flow(value) {
        let updated = remove_from_flow_line(&lines[key_line], candidates)
            .ok_or_else(|| anyhow::anyhow!("could not edit projects: list as text"))?;
        lines[key_line] = updated;
        return Ok(ProjectsEdit::Updated(join_lines(&lines, newline_at_end)));
    }
    if !value.is_empty() && !is_empty_list_value(value) {
        anyhow::bail!("could not edit projects: list as text");
    }
    let mut removed = false;
    let mut idx = key_line + 1;
    while idx < lines.len() {
        if is_section_end(&lines[idx]) {
            break;
        }
        if let Some(item) = block_item_value(&lines[idx]) {
            if candidates.contains(&item.as_str()) {
                lines.remove(idx);
                removed = true;
                continue;
            }
        }
        idx += 1;
    }
    if !removed {
        anyhow::bail!("could not edit projects: list as text");
    }
    Ok(ProjectsEdit::Updated(join_lines(&lines, newline_at_end)))
}

fn append_projects_block(lines: &mut Vec<String>, path: &str) {
    if lines.last().is_some_and(|line| !line.is_empty()) {
        lines.push(String::new());
    }
    lines.push("projects:".to_string());
    lines.push(format!("  - {}", format_yaml_string(path)));
}

fn find_root_projects(lines: &[String]) -> Option<usize> {
    lines.iter().position(|line| projects_value(line).is_some())
}

fn projects_value(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("projects:")?;
    if rest.is_empty() || rest.starts_with([' ', '\t']) {
        Some(rest)
    } else {
        None
    }
}

fn insert_index(lines: &[String], key_line: usize) -> usize {
    let mut last = key_line;
    for (idx, line) in lines.iter().enumerate().skip(key_line + 1) {
        if is_section_end(line) {
            break;
        }
        if block_item_value(line).is_some() || is_commented_list_item(line) || is_continuation(line)
        {
            last = idx;
        }
    }
    last
}

fn detect_indent(lines: &[String], key_line: usize) -> String {
    for line in lines.iter().skip(key_line + 1) {
        if is_section_end(line) {
            break;
        }
        if block_item_value(line).is_some() || is_commented_list_item(line) {
            let trimmed = line.trim_start();
            return line[..line.len() - trimmed.len()].to_string();
        }
    }
    "  ".to_string()
}

fn is_section_end(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return false;
    }
    if trimmed == "---" || trimmed == "..." {
        return true;
    }
    if line.starts_with([' ', '\t']) {
        return false;
    }
    if trimmed.starts_with('-') {
        return false;
    }
    trimmed.contains(':')
}

fn block_item_value(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }
    let rest = trimmed.strip_prefix('-')?;
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    Some(parse_scalar(rest))
}

fn is_commented_list_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix('#') else {
        return false;
    };
    rest.trim_start().starts_with('-')
}

fn is_continuation(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
        return false;
    }
    line.starts_with([' ', '\t'])
}

fn is_empty_list_value(value: &str) -> bool {
    matches!(value, "" | "[]" | "~" | "null" | "Null" | "NULL")
}

fn is_complete_flow(value: &str) -> bool {
    value.starts_with('[') && value.ends_with(']')
}

fn add_to_flow_line(line: &str, path: &str) -> Option<String> {
    let rest = projects_value(line)?;
    let (value, comment) = split_value_and_comment(rest);
    let open = value.find('[')?;
    let close = value.rfind(']')?;
    let inner = value[open + 1..close].trim();
    let formatted = format_yaml_string(path);
    let new_inner = if inner.is_empty() {
        formatted
    } else {
        format!("{inner}, {formatted}")
    };
    Some(format!("projects:{}[{new_inner}]{comment}", &value[..open]))
}

fn remove_from_flow_line(line: &str, candidates: &[&str]) -> Option<String> {
    let rest = projects_value(line)?;
    let (value, comment) = split_value_and_comment(rest);
    let open = value.find('[')?;
    let close = value.rfind(']')?;
    let inner = &value[open + 1..close];
    let items = parse_flow_items(inner)?;
    let remaining: Vec<String> = items
        .into_iter()
        .filter(|item| !candidates.contains(&item.as_str()))
        .map(|item| format_yaml_string(&item))
        .collect();
    Some(format!(
        "projects:{}[{}]{comment}",
        &value[..open],
        remaining.join(", ")
    ))
}

fn parse_flow_items(inner: &str) -> Option<Vec<String>> {
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_double => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            ',' if !in_single && !in_double => {
                items.push(parse_scalar(&current));
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if in_single || in_double {
        return None;
    }
    if !current.trim().is_empty() {
        items.push(parse_scalar(&current));
    }
    Some(items)
}

fn split_value_and_comment(s: &str) -> (&str, &str) {
    match inline_comment_index(s) {
        Some(hash_idx) => {
            let value = s[..hash_idx].trim_end();
            let comment = &s[value.len()..];
            (value, comment)
        }
        None => (s, ""),
    }
}

fn inline_comment_index(s: &str) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' if in_double => {
                chars.next();
            }
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single
                && !in_double
                && (i == 0
                    || s.as_bytes()
                        .get(i - 1)
                        .is_some_and(|b| b.is_ascii_whitespace())) =>
            {
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

fn parse_scalar(s: &str) -> String {
    let s = if let Some(i) = inline_comment_index(s) {
        s[..i].trim()
    } else {
        s.trim()
    };
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        return s[1..s.len() - 1]
            .replace("\\\\", "\\")
            .replace("\\\"", "\"");
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return s[1..s.len() - 1].replace("''", "'");
    }
    s.to_string()
}

fn format_yaml_string(s: &str) -> String {
    if needs_yaml_quotes(s) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn needs_yaml_quotes(s: &str) -> bool {
    s.is_empty()
        || s.starts_with([' ', '\t', '-'])
        || s.ends_with([' ', '\t'])
        || matches!(
            s,
            "null" | "Null" | "NULL" | "~" | "true" | "false" | "yes" | "no"
        )
        || s.bytes().any(|b| {
            matches!(
                b,
                b':' | b'#'
                    | b'{'
                    | b'}'
                    | b'['
                    | b']'
                    | b','
                    | b'&'
                    | b'*'
                    | b'!'
                    | b'|'
                    | b'>'
                    | b'<'
                    | b'='
                    | b'%'
                    | b'@'
                    | b'`'
                    | b'\''
                    | b'"'
                    | b'\\'
            )
        })
}

fn split_lines(content: &str) -> (Vec<String>, bool) {
    if content.is_empty() {
        return (Vec::new(), false);
    }
    let newline_at_end = content.ends_with('\n');
    let body = content.strip_suffix('\n').unwrap_or(content);
    (
        body.split('\n').map(str::to_string).collect(),
        newline_at_end,
    )
}

fn join_lines(lines: &[String], newline_at_end: bool) -> String {
    let mut out = lines.join("\n");
    if (newline_at_end || !out.is_empty()) && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTED_CONFIG: &str = "\
# Orch global configuration
# See: https://github.com/gabrielkoerich/orch

projects:
  - /Users/me/Projects/orch
  # - /Users/me/Projects/old-app

# Agents to discover in PATH
agents:
  - claude
  - codex
  # - glm
  # - olm  # Ollama via Claude-compatible CLI

router:
  mode: \"round_robin\"
  # weights:
  #   claude: 0.25
  #   codex: 0.1
  # pool:
  #   - claude:haiku
  #   - opencode:free
";

    fn updated(edit: ProjectsEdit) -> String {
        match edit {
            ProjectsEdit::Updated(s) => s,
            ProjectsEdit::Unchanged => panic!("expected Updated"),
        }
    }

    fn comment_lines(content: &str) -> Vec<&str> {
        content
            .lines()
            .filter(|line| line.trim_start().starts_with('#'))
            .collect()
    }

    fn blank_line_count(content: &str) -> usize {
        content.lines().filter(|line| line.is_empty()).count()
    }

    #[test]
    fn project_add_preserves_comments_and_blank_lines() {
        let after = updated(add_project(COMMENTED_CONFIG, "/Users/me/Projects/new").unwrap());
        assert_eq!(comment_lines(COMMENTED_CONFIG), comment_lines(&after));
        assert_eq!(blank_line_count(COMMENTED_CONFIG), blank_line_count(&after));
        assert!(after.contains("  - /Users/me/Projects/new\n"));
        assert!(after.contains("  - /Users/me/Projects/orch\n"));
        assert_eq!(
            after,
            "\
# Orch global configuration
# See: https://github.com/gabrielkoerich/orch

projects:
  - /Users/me/Projects/orch
  # - /Users/me/Projects/old-app
  - /Users/me/Projects/new

# Agents to discover in PATH
agents:
  - claude
  - codex
  # - glm
  # - olm  # Ollama via Claude-compatible CLI

router:
  mode: \"round_robin\"
  # weights:
  #   claude: 0.25
  #   codex: 0.1
  # pool:
  #   - claude:haiku
  #   - opencode:free
"
        );
    }

    #[test]
    fn project_add_existing_path_changes_nothing() {
        let edit = add_project(COMMENTED_CONFIG, "/Users/me/Projects/orch").unwrap();
        assert_eq!(edit, ProjectsEdit::Unchanged);
    }

    #[test]
    fn project_add_quoted_duplicate_changes_nothing() {
        let config = "projects:\n  - \"/Users/me/Projects/orch\"\n# keep me\n";
        let edit = add_project(config, "/Users/me/Projects/orch").unwrap();
        assert_eq!(edit, ProjectsEdit::Unchanged);
    }

    #[test]
    fn project_remove_preserves_comments() {
        let after =
            updated(remove_project(COMMENTED_CONFIG, &["/Users/me/Projects/orch"]).unwrap());
        assert_eq!(comment_lines(COMMENTED_CONFIG), comment_lines(&after));
        assert!(!after.contains("  - /Users/me/Projects/orch\n"));
        assert!(after.contains("  # - /Users/me/Projects/old-app\n"));
        assert!(after.contains("  # - glm\n"));
        assert_eq!(
            after,
            "\
# Orch global configuration
# See: https://github.com/gabrielkoerich/orch

projects:
  # - /Users/me/Projects/old-app

# Agents to discover in PATH
agents:
  - claude
  - codex
  # - glm
  # - olm  # Ollama via Claude-compatible CLI

router:
  mode: \"round_robin\"
  # weights:
  #   claude: 0.25
  #   codex: 0.1
  # pool:
  #   - claude:haiku
  #   - opencode:free
"
        );
    }

    #[test]
    fn project_remove_missing_path_changes_nothing() {
        let edit = remove_project(COMMENTED_CONFIG, &["/nope"]).unwrap();
        assert_eq!(edit, ProjectsEdit::Unchanged);
    }

    #[test]
    fn project_add_empty_flow_list_keeps_surrounding_comments() {
        let config = "\
# header

projects: []

# Agents
agents:
  # - glm
  - claude
";
        let after = updated(add_project(config, "/tmp/app").unwrap());
        assert_eq!(comment_lines(config), comment_lines(&after));
        assert_eq!(
            after,
            "\
# header

projects:
  - /tmp/app

# Agents
agents:
  # - glm
  - claude
"
        );
    }

    #[test]
    fn project_add_appends_block_when_projects_key_missing() {
        let config = "# just a comment\n\nrouter:\n  mode: llm\n";
        let after = updated(add_project(config, "/tmp/app").unwrap());
        assert!(after.contains("# just a comment\n"));
        assert!(after.ends_with("projects:\n  - /tmp/app\n"));
    }

    #[test]
    fn project_add_rejects_non_list_projects() {
        let err = add_project("projects: nope\n", "/tmp/app").unwrap_err();
        assert!(err.to_string().contains("projects is not a list"));
    }

    #[test]
    fn project_add_appends_to_flow_list() {
        let config = "# keep\nprojects: [/a, /b]\n# tail\n";
        let after = updated(add_project(config, "/c").unwrap());
        assert_eq!(after, "# keep\nprojects: [/a, /b, /c]\n# tail\n");
    }

    #[test]
    fn project_remove_from_flow_list() {
        let config = "# keep\nprojects: [/a, /b]\n# tail\n";
        let after = updated(remove_project(config, &["/a"]).unwrap());
        assert_eq!(after, "# keep\nprojects: [/b]\n# tail\n");
    }
}
