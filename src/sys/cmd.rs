//! Thin wrapper around `std::process::Command` / `tokio::process::Command`
//! that attaches the binary path to any spawn error so "No such file or
//! directory" always says *which* file was missing.

use std::ffi::OsStr;
use std::path::PathBuf;

use tokio::process;

use std::future::Future;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Output;

/// Extension trait that maps spawn errors to include the program path.
pub trait CommandErrorContext {
    /// Like `.output()` but the error includes the program name.
    fn output_with_context(&mut self) -> impl Future<Output = anyhow::Result<Output>>;
}

impl CommandErrorContext for process::Command {
    async fn output_with_context(&mut self) -> anyhow::Result<Output> {
        let prog = program_name(self.as_std());
        self.output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to execute `{prog}`: {e}"))
    }
}

/// Sync version for `std::process::Command`.
pub trait SyncCommandErrorContext {
    fn output_with_context(&mut self) -> anyhow::Result<Output>;
    fn status_with_context(&mut self) -> anyhow::Result<ExitStatus>;
}

impl SyncCommandErrorContext for Command {
    fn output_with_context(&mut self) -> anyhow::Result<Output> {
        let prog = program_name(self);
        self.output()
            .map_err(|e| anyhow::anyhow!("failed to execute `{prog}`: {e}"))
    }

    fn status_with_context(&mut self) -> anyhow::Result<ExitStatus> {
        let prog = program_name(self);
        self.status()
            .map_err(|e| anyhow::anyhow!("failed to execute `{prog}`: {e}"))
    }
}

/// `kill(2)`. A negative `pid` addresses the process group with that id
#[cfg(unix)]
pub fn raw_kill(pid: i32, sig: i32) -> i32 {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // SAFETY: both arguments are plain integers and kill has no other effect on this process
    unsafe { kill(pid, sig) }
}

/// SIGKILL the process group led by `pgid`, a child started with `process_group(0)`. The group is
/// the only handle on grandchildren, which outlive a child killed alone
#[cfg(unix)]
pub fn kill_process_group(pgid: u32) {
    const SIGKILL: i32 = 9;
    let Ok(pgid) = i32::try_from(pgid) else {
        return;
    };
    let _ = raw_kill(-pgid, SIGKILL);
}

/// Extract the program name from a `std::process::Command`.
fn program_name(cmd: &Command) -> String {
    let prog: &OsStr = cmd.get_program();
    let path = PathBuf::from(prog);
    path.display().to_string()
}
