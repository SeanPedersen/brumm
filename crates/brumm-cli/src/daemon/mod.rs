//! `brumm-cli daemon`: one process per OS user and data root that hosts several profiles and
//! serves JSON-RPC (see `protocol.md`) on a local socket.
//!
//! - Holds `<root>/daemon.lock` for its whole life, so of two racing daemons one exits at once.
//! - Each connection gets a [`Session`]; its requests run concurrently (a long `message.wait`
//!   does not hold up a `send`), answers and notifications share one bounded outbound queue.
//! - `--idle-exit`: profiles nobody uses close after that long, and the daemon exits once no
//!   client has been connected for that long. Closing drains unsent messages to the relays.

pub mod socket;
pub mod spawn;

use std::{
    fs::{self, File, TryLockError},
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Notify, mpsc},
    task::JoinSet,
};

use crate::{
    host::{
        Host, HostConfig,
        methods,
        session::{DaemonControl, Session},
    },
    paths,
    protocol::{self, JSONRPC, Request, Response, RpcError, code},
};

/// Answers and notifications waiting for a slow client.
pub const OUTBOUND_BUFFER: usize = 256;
/// How often idle profiles and the idle exit are checked: a quarter of the idle time, within
/// these bounds, so a profile closes soon after it became idle.
const MAX_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const MIN_SWEEP_INTERVAL: Duration = Duration::from_millis(20);
const SWEEPS_PER_IDLE_TIME: u32 = 4;

fn sweep_interval(idle_exit: Option<Duration>) -> Duration {
    idle_exit.map_or(MAX_SWEEP_INTERVAL, |idle| {
        (idle / SWEEPS_PER_IDLE_TIME).clamp(MIN_SWEEP_INTERVAL, MAX_SWEEP_INTERVAL)
    })
}

pub struct DaemonOptions {
    pub host: HostConfig,
    pub socket: PathBuf,
    pub idle_exit: Option<Duration>,
}

struct State {
    started: Instant,
    connections: AtomicUsize,
    /// When the last client disconnected (or the daemon started).
    quiet_since: Mutex<Instant>,
    stop: Notify,
}

impl DaemonControl for State {
    fn status(&self) -> serde_json::Value {
        json!({
            "pid": std::process::id(),
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "connections": self.connections.load(Ordering::Relaxed),
        })
    }

    fn stop(&self) {
        self.stop.notify_one();
    }
}

impl State {
    fn quiet_for(&self) -> Option<Duration> {
        if self.connections.load(Ordering::Relaxed) > 0 {
            return None;
        }
        Some(self.quiet_since.lock().unwrap_or_else(PoisonError::into_inner).elapsed())
    }
}

/// Counts a connection while it lives.
struct Connected(Arc<State>);

impl Connected {
    fn new(state: Arc<State>) -> Self {
        state.connections.fetch_add(1, Ordering::Relaxed);
        Self(state)
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        *self.0.quiet_since.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        self.0.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Held for the daemon's life; the OS releases it however the process ends.
fn lock_root(root: &std::path::Path) -> Result<File> {
    fs::create_dir_all(root).context("create data dir")?;
    let file = File::create(paths::daemon_lock(root)).context("open daemon lock")?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!("a brumm daemon is already running for {}", root.display())
        }
        Err(TryLockError::Error(err)) => Err(err).context("lock daemon"),
    }
}

pub async fn run(options: DaemonOptions) -> Result<()> {
    let _lock = lock_root(&options.host.root)?;
    let listener = socket::bind(&options.socket)
        .with_context(|| format!("listen on {}", options.socket.display()))?;
    tracing::info!(socket = %options.socket.display(), pid = std::process::id(), "daemon listening");
    let host = Host::new(options.host);
    let state = Arc::new(State {
        started: Instant::now(),
        connections: AtomicUsize::new(0),
        quiet_since: Mutex::new(Instant::now()),
        stop: Notify::new(),
    });
    if let Some(idle) = options.idle_exit {
        let host = host.clone();
        tokio::spawn(async move {
            let mut sweep = tokio::time::interval(sweep_interval(Some(idle)));
            loop {
                sweep.tick().await;
                host.close_idle(idle).await;
            }
        });
    }
    let mut sweep = tokio::time::interval(sweep_interval(options.idle_exit));
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    let connected = Connected::new(state.clone());
                    let control: Arc<dyn DaemonControl> = state.clone();
                    tokio::spawn(serve(stream, host.clone(), control, connected));
                }
                Err(err) => tracing::warn!("accept failed: {err}"),
            },
            _ = state.stop.notified() => break,
            _ = shutdown_signal() => break,
            _ = sweep.tick() => {
                let idle_exit = options
                    .idle_exit
                    .zip(state.quiet_for())
                    .is_some_and(|(idle, quiet)| quiet >= idle);
                if idle_exit {
                    tracing::info!("no clients for the idle time, exiting");
                    break;
                }
            }
        }
    }
    // New clients get "connection refused" and start a fresh daemon, which waits on each
    // profile lock until this one has drained it.
    listener.remove();
    host.shutdown().await;
    tracing::info!("daemon stopped");
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// One connection: reads requests, runs each in its own task, writes answers and
/// notifications through the bounded outbound queue.
pub async fn serve<S>(stream: S, host: Arc<Host>, control: Arc<dyn DaemonControl>, connected: impl Send + 'static)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let _connected = connected;
    let (read, mut write) = tokio::io::split(stream);
    let (outbound, mut queued) = mpsc::channel::<String>(OUTBOUND_BUFFER);
    let session = Session::new(host, outbound.clone(), Some(control));
    let writer = tokio::spawn(async move {
        while let Some(mut line) = queued.recv().await {
            line.push('\n');
            if write.write_all(line.as_bytes()).await.is_err() {
                return;
            }
        }
    });
    let mut reader = BufReader::new(read);
    let mut line = Vec::new();
    let mut requests = JoinSet::new();
    loop {
        match protocol::read_line(&mut reader, &mut line).await {
            Ok(Some(())) if line.iter().all(u8::is_ascii_whitespace) => {}
            Ok(Some(())) => match serde_json::from_slice::<Request>(&line) {
                Ok(request) if request.jsonrpc == JSONRPC => {
                    let session = session.clone();
                    let outbound = outbound.clone();
                    requests.spawn(async move {
                        let outcome = methods::call(&session, &request.method, request.params).await;
                        if let Some(id) = request.id {
                            let response = Response::new(id, outcome);
                            let _ = outbound.send(encode(&response)).await;
                        }
                    });
                }
                Ok(request) => {
                    let error = RpcError::new(code::INVALID_REQUEST, "jsonrpc must be \"2.0\"");
                    let id = request.id.unwrap_or_default();
                    let _ = outbound.send(encode(&Response::new(id, Err(error)))).await;
                }
                Err(err) => {
                    let error = RpcError::new(code::PARSE_ERROR, err.to_string());
                    let _ = outbound
                        .send(encode(&Response::new(serde_json::Value::Null, Err(error))))
                        .await;
                }
            },
            Ok(None) => break,
            Err(err) => {
                let error = RpcError::new(code::PARSE_ERROR, err.to_string());
                let _ = outbound
                    .send(encode(&Response::new(serde_json::Value::Null, Err(error))))
                    .await;
                break;
            }
        }
        // Reap finished requests so the set does not grow with the connection's age.
        while requests.try_join_next().is_some() {}
    }
    // The client is gone: nobody waits for the answers of what is still running.
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    drop(session);
    drop(outbound);
    let _ = writer.await;
}

fn encode(response: &Response) -> String {
    serde_json::to_string(response).unwrap_or_else(|err| {
        format!(r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":{},"message":"{err}"}}}}"#, code::FAILED)
    })
}
