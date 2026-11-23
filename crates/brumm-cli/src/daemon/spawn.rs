//! Find the daemon for a data root, or start one. Race-safe: of several clients that start a
//! daemon at once, every daemon but one fails to take `<root>/daemon.lock` and exits, and all
//! clients connect to the one that won.

use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{paths, protocol::PROTOCOL_VERSION, remote::Remote, text::duration_millis};

/// A freshly started daemon binds within milliseconds; opening profiles comes later.
const START_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_POLL: Duration = Duration::from_millis(50);
/// `--idle-exit` of daemons started on demand: long enough for a burst of CLI calls.
pub const AUTO_IDLE_EXIT: Duration = Duration::from_secs(60);
/// How much of the log a failed start shows.
const LOG_TAIL_BYTES: usize = 2000;

/// How to reach (and, if needed, start) the daemon of a data root.
#[derive(Debug, Clone)]
pub struct Target {
    pub root: PathBuf,
    pub socket: PathBuf,
    /// Passed on to a daemon this starts.
    pub relays: Vec<String>,
    pub retry_interval: Option<Duration>,
    pub idle_exit: Option<Duration>,
}

/// The running daemon, if there is one (and it speaks our protocol).
pub async fn connect(target: &Target) -> Result<Option<Remote>> {
    let Ok(remote) = Remote::connect(&target.socket).await else {
        return Ok(None);
    };
    let status = remote
        .call("daemon.status", json!({}))
        .await
        .context("ask the daemon for its status")?;
    check_protocol(&status)?;
    Ok(Some(remote))
}

fn check_protocol(status: &Value) -> Result<()> {
    let protocol = status["protocol"].as_u64().unwrap_or_default();
    if protocol != u64::from(PROTOCOL_VERSION) {
        bail!(
            "the running daemon speaks protocol v{protocol}, this brumm-cli v{PROTOCOL_VERSION}; stop it with `brumm-cli daemon stop`"
        );
    }
    Ok(())
}

/// Connects to the daemon, starting it first if none runs.
pub async fn ensure(target: &Target) -> Result<Remote> {
    if let Some(remote) = connect(target).await? {
        return Ok(remote);
    }
    start(target)?;
    let started = Instant::now();
    loop {
        if let Some(remote) = connect(target).await? {
            return Ok(remote);
        }
        if started.elapsed() > START_TIMEOUT {
            bail!(
                "the daemon did not start within {START_TIMEOUT:?}; its log ({}):\n{}",
                paths::daemon_log(&target.root).display(),
                log_tail(&target.root)
            );
        }
        tokio::time::sleep(CONNECT_POLL).await;
    }
}

/// Starts a detached daemon process (it outlives this one) logging to `<root>/daemon.log`.
fn start(target: &Target) -> Result<()> {
    std::fs::create_dir_all(&target.root).context("create data dir")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths::daemon_log(&target.root))
        .context("open daemon log")?;
    let binary = std::env::current_exe().context("find own binary")?;
    let mut command = Command::new(binary);
    command
        .arg("--data-dir")
        .arg(&target.root)
        .arg("--socket")
        .arg(&target.socket);
    for relay in &target.relays {
        command.arg("--relay").arg(relay);
    }
    if let Some(retry) = target.retry_interval {
        command.arg("--retry-interval").arg(format!("{}ms", duration_millis(retry)));
    }
    command.arg("daemon").arg("run");
    if let Some(idle) = target.idle_exit {
        command.arg("--idle-exit").arg(format!("{}ms", duration_millis(idle)));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        // Settings for this run only; the daemon gets them as arguments.
        .env_remove("BRUMM_NO_DAEMON");
    detach(&mut command);
    command.spawn().context("start the daemon")?;
    Ok(())
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Own process group: Ctrl+C in the terminal that started it does not stop it.
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

fn log_tail(root: &Path) -> String {
    let log = std::fs::read(paths::daemon_log(root)).unwrap_or_default();
    let start = log.len().saturating_sub(LOG_TAIL_BYTES);
    String::from_utf8_lossy(&log[start..]).into_owned()
}
