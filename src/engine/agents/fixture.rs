//! Agent output fixtures under `tests/fixtures/`, written by `scripts/record-agent-fixture.py`.
//! A `# key: value` header holds the exit code and stderr, the rest is the agent's stdout

use crate::engine::runner::session::SessionOutput;

/// One recorded run, split into what the runner sees
pub(crate) struct Fixture {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Fixture {
    pub(crate) fn session_output(&self) -> SessionOutput {
        SessionOutput {
            exit_code: self.exit_code,
            raw_stdout: self.stdout.clone(),
            raw_stderr: self.stderr.clone(),
            elapsed_secs: None,
            sandbox_error: None,
        }
    }
}

/// Split a fixture into its header fields and stdout. A file with no header is all stdout
pub(crate) fn load(raw: &str) -> Fixture {
    let mut exit_code = 0;
    let mut stderr = String::new();
    let mut lines = raw.lines().peekable();
    while let Some(field) = lines.peek().and_then(|l| l.strip_prefix("# ")) {
        if let Some(code) = field.strip_prefix("exit: ") {
            exit_code = code.trim().parse().expect("fixture exit code is a number");
        } else if let Some(line) = field.strip_prefix("stderr: ") {
            stderr.push_str(line);
            stderr.push('\n');
        }
        lines.next();
    }
    let mut stdout = lines.collect::<Vec<_>>().join("\n");
    stdout.push('\n');
    Fixture {
        exit_code,
        stdout,
        stderr,
    }
}
