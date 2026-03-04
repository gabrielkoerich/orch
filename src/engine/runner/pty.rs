//! PTY-based agent runner helpers.
//!
//! Spawns agent CLIs under a pseudo-terminal, streams output to tmux,
//! and writes attempt artifacts (output/stderr/exit status).

use crate::tmux::TmuxManager;
use anyhow::Context;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// PTY invocation configuration.
pub struct PtyInvocation {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
}

/// Handle to a spawned PTY child and IO streams.
pub struct PtyHandle {
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

/// Attach context for streaming PTY output into tmux and artifacts.
pub struct PtyAttachSession {
    pub tmux: TmuxManager,
    pub session: String,
    pub output_path: PathBuf,
    pub stderr_path: PathBuf,
    pub exit_path: PathBuf,
    pub timeout_seconds: u64,
}

impl PtyHandle {
    pub fn write_stdin(&mut self, data: &[u8]) -> anyhow::Result<()> {
        if !data.is_empty() {
            self.writer.write_all(data)?;
        }
        self.writer.flush()?;
        Ok(())
    }
}

/// Spawn an agent CLI under a PTY.
pub fn spawn_agent_in_pty(inv: &PtyInvocation) -> anyhow::Result<PtyHandle> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening PTY")?;

    let mut cmd = CommandBuilder::new(inv.program.clone());
    cmd.args(&inv.args);
    cmd.cwd(inv.cwd.clone());

    for (k, v) in &inv.env {
        cmd.env(k, v);
    }
    // Ensure we don't inherit CLAUDECODE when running under other agents.
    cmd.env("CLAUDECODE", "");

    let child = pair
        .slave
        .spawn_command(cmd)
        .context("spawning PTY child")?;

    let reader = pair
        .master
        .try_clone_reader()
        .context("cloning PTY reader")?;
    let writer = pair.master.take_writer().context("taking PTY writer")?;

    Ok(PtyHandle {
        child: Arc::new(Mutex::new(child)),
        reader,
        writer,
    })
}

/// Attach a PTY stream to a tmux session and write artifacts.
///
/// This spawns background tasks that:
/// - write PTY output to `output_path` and `stderr_path`
/// - forward output into the tmux session pane
/// - record exit code into `exit_path`
pub fn attach_pty_to_tmux(pty: PtyHandle, session: PtyAttachSession) -> anyhow::Result<()> {
    let PtyHandle {
        child,
        mut reader,
        writer: _,
    } = pty;

    let PtyAttachSession {
        tmux,
        session,
        output_path,
        stderr_path,
        exit_path,
        timeout_seconds,
    } = session;

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let tmux_clone = tmux.clone();
    let session_clone = session.clone();

    tokio::spawn(async move {
        while let Some(chunk) = rx.recv().await {
            if let Err(err) = tmux_clone.send_text(&session_clone, &chunk).await {
                tracing::debug!(session = %session_clone, error = %err, "tmux send-keys failed");
            }
        }
    });

    // Stream stdout from PTY in a blocking task.
    let output_path_clone = output_path.clone();
    let stderr_path_clone = stderr_path.clone();
    let stream_handle = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        stream_pty_output(
            &mut reader,
            &output_path_clone,
            &stderr_path_clone,
            Some(tx),
        )
    });

    let child_handle = child.clone();
    tokio::spawn(async move {
        let wait_handle = tokio::task::spawn_blocking(move || {
            let mut guard = child_handle.lock().unwrap();
            guard.wait().map(|status| status.exit_code() as i32)
        });

        let exit_code = if timeout_seconds > 0 {
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_seconds), wait_handle)
                .await
            {
                Ok(Ok(Ok(code))) => code,
                Ok(Ok(Err(err))) => {
                    tracing::error!(error = %err, "PTY child wait failed");
                    -1
                }
                Ok(Err(err)) => {
                    tracing::error!(error = %err, "PTY wait task failed");
                    -1
                }
                Err(_) => {
                    if let Ok(mut guard) = child.lock() {
                        let _ = guard.kill();
                    }
                    -1
                }
            }
        } else {
            match wait_handle.await {
                Ok(Ok(code)) => code,
                Ok(Err(err)) => {
                    tracing::error!(error = %err, "PTY child wait failed");
                    -1
                }
                Err(err) => {
                    tracing::error!(error = %err, "PTY wait task failed");
                    -1
                }
            }
        };

        let _ = stream_handle.await;

        if let Err(err) = std::fs::write(&exit_path, exit_code.to_string()) {
            tracing::warn!(error = %err, path = %exit_path.display(), "failed to write PTY exit");
        }

        let _ = tmux.kill_session(&session).await;
    });

    Ok(())
}

pub(crate) fn stream_pty_output(
    reader: &mut dyn Read,
    output_path: &PathBuf,
    stderr_path: &PathBuf,
    sender: Option<mpsc::UnboundedSender<String>>,
) -> anyhow::Result<()> {
    let mut out = std::io::BufWriter::new(
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(output_path)?,
    );
    let mut err = std::io::BufWriter::new(
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(stderr_path)?,
    );

    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        // PTY combines stdout/stderr; mirror to stderr for compatibility.
        err.write_all(&buf[..n])?;

        if let Some(ref tx) = sender {
            let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = tx.send(chunk);
        }
    }

    out.flush()?;
    err.flush()?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn run_pty_to_completion(
    pty: PtyHandle,
    output_path: &PathBuf,
    stderr_path: &PathBuf,
) -> anyhow::Result<i32> {
    let PtyHandle {
        child,
        mut reader,
        writer,
    } = pty;
    drop(writer);

    stream_pty_output(&mut reader, output_path, stderr_path, None)?;

    let mut guard = child.lock().unwrap();
    let status = guard.wait().context("waiting for PTY child")?;
    Ok(status.exit_code() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::tempdir;

    #[test]
    fn pty_writes_output_and_stderr_artifacts_without_runner_script() {
        let dir = tempdir().expect("tempdir");
        let output_path = dir.path().join("output.json");
        let stderr_path = dir.path().join("stderr.txt");

        let invocation = PtyInvocation {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf 'out'; printf 'err' 1>&2".to_string(),
            ],
            cwd: dir.path().to_path_buf(),
            env: HashMap::new(),
        };

        let pty = spawn_agent_in_pty(&invocation).expect("spawn pty");
        let exit_code = run_pty_to_completion(pty, &output_path, &stderr_path).expect("run pty");

        let stdout = std::fs::read_to_string(&output_path).expect("read stdout");
        let stderr = std::fs::read_to_string(&stderr_path).expect("read stderr");

        assert_eq!(exit_code, 0);
        assert!(stdout.contains("out"));
        assert!(stderr.contains("err"));
        assert!(!dir.path().join("runner.sh").exists());
    }
}
