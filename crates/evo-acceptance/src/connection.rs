//! Connection — the harness's mechanism for executing a scenario
//! command on the target. SSH is the default for remote targets;
//! local-exec is for in-process or co-located dev-box runs.
//!
//! Modelled as an enum rather than a `dyn` trait because async
//! methods are not object-safe in stable Rust. New transports
//! (serial console for embedded boards, container exec, etc.) add
//! a variant and a `match` arm.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::error::{HarnessError, Result};
use crate::target::{ConnectionType, SshConfig, TargetDescriptor};

#[derive(Debug)]
pub struct CommandResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

pub enum Connection {
    Ssh(SshConfig),
    Local,
}

pub fn build(target: &TargetDescriptor) -> Result<Connection> {
    match target.connection_type {
        ConnectionType::Ssh => Ok(Connection::Ssh(target.ssh()?.clone())),
        ConnectionType::Local => Ok(Connection::Local),
    }
}

impl Connection {
    /// Execute `cmd` on the target with the given timeout. Returns
    /// captured stdout / stderr and the exit code. If the command
    /// exceeds the timeout, the returned `CommandResult.timed_out`
    /// is true and `exit_code` is set to a sentinel (-1).
    pub async fn run(
        &self,
        cmd: &str,
        timeout_dur: Duration,
    ) -> Result<CommandResult> {
        match self {
            Self::Ssh(config) => run_ssh(config, cmd, timeout_dur).await,
            Self::Local => run_local(cmd, timeout_dur).await,
        }
    }
}

async fn run_ssh(
    config: &SshConfig,
    cmd: &str,
    timeout_dur: Duration,
) -> Result<CommandResult> {
    let mut ssh = Command::new("ssh");
    ssh.arg("-p")
        .arg(config.port.to_string())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg("ConnectTimeout=15");
    if let Some(key_path) = &config.key_path {
        ssh.arg("-i").arg(key_path);
    }
    ssh.arg(format!("{}@{}", config.user, config.host)).arg(cmd);
    run_command(ssh, timeout_dur).await
}

async fn run_local(cmd: &str, timeout_dur: Duration) -> Result<CommandResult> {
    let mut sh = Command::new("sh");
    sh.arg("-c").arg(cmd);
    run_command(sh, timeout_dur).await
}

async fn run_command(
    mut cmd: Command,
    timeout_dur: Duration,
) -> Result<CommandResult> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let cmd_label = format!("{:?}", cmd.as_std());
    let mut child =
        cmd.spawn().map_err(|source| HarnessError::CommandSpawn {
            command: cmd_label,
            source,
        })?;

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    let read_pair = async move {
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut h) = stdout_handle {
            let _ = h.read_to_string(&mut stdout).await;
        }
        if let Some(mut h) = stderr_handle {
            let _ = h.read_to_string(&mut stderr).await;
        }
        (stdout, stderr)
    };

    let combined = async {
        let pair = read_pair.await;
        let status = child.wait().await;
        (pair, status)
    };

    match timeout(timeout_dur, combined).await {
        Ok(((stdout, stderr), Ok(status))) => Ok(CommandResult {
            exit_code: status.code().unwrap_or(-1),
            stdout,
            stderr,
            timed_out: false,
        }),
        Ok(((stdout, stderr), Err(err))) => Err(HarnessError::ConnectionFailed {
            detail: format!("child process error after capture: {err}; stdout {stdout:?} stderr {stderr:?}"),
        }),
        Err(_elapsed) => Ok(CommandResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("evo-acceptance: command exceeded timeout of {}s", timeout_dur.as_secs()),
            timed_out: true,
        }),
    }
}
