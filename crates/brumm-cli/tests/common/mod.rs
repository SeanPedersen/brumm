//! Harness for brumm-cli end-to-end tests: runs the real binary on a fresh data root, talks to
//! its daemon over the socket, and runs "the app" as an in-process brumm-host client.
#![allow(dead_code)] // each test binary uses a different subset of the harness

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use brumm_cli::{paths, remote::Remote};
use brumm_core::node::NodeConfig;
use brumm_host::Client;
use serde_json::{Value, json};

pub const BINARY: &str = env!("CARGO_BIN_EXE_brumm-cli");
/// Resends in tests: twice a second.
const TEST_RETRY: &str = "500ms";
const TEST_RETRY_INTERVAL: Duration = Duration::from_millis(500);
/// Nothing listens here: tests without a relay get no offline delivery.
pub const NO_RELAY: &str = "ws://127.0.0.1:9";
pub const POLL: Duration = Duration::from_millis(100);
pub const PATIENCE: Duration = Duration::from_secs(45);

pub fn unique(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{name}-{nanos}")
}

/// A data root plus the relays its profiles receive on.
pub struct Env {
    pub root: PathBuf,
    pub relays: Vec<String>,
}

impl Env {
    pub fn new(name: &str, relays: Vec<String>) -> Self {
        let _ = tracing_subscriber_init();
        Self {
            root: PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(unique(name)),
            relays,
        }
    }

    pub fn socket(&self) -> PathBuf {
        paths::socket_path(&self.root, false)
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BINARY);
        command
            .args(args)
            .env("BRUMM_DATA_DIR", &self.root)
            .env("BRUMM_RELAYS", self.relays.join(","))
            .env("BRUMM_RETRY_INTERVAL", TEST_RETRY)
            .env_remove("BRUMM_PROFILE")
            .env_remove("BRUMM_SOCKET")
            .env_remove("BRUMM_NO_DAEMON")
            .stdin(Stdio::null());
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run brumm-cli")
    }

    /// Runs with `input` on stdin.
    pub fn run_with_stdin(&self, args: &[&str], input: &[u8]) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run brumm-cli");
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    /// Runs and expects success; returns stdout.
    pub fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert_ok(&output, args);
        String::from_utf8(output.stdout).unwrap()
    }

    /// `--json` output of a successful run, one value per line.
    pub fn json(&self, args: &[&str]) -> Vec<Value> {
        let mut with_json = vec!["--json"];
        with_json.extend_from_slice(args);
        self.ok(&with_json)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    pub fn profile_db(&self, profile: &str) -> PathBuf {
        paths::profile_dir(&self.root, profile).unwrap().join("brumm.db")
    }

    /// Starts the daemon (or finds it) and connects.
    pub async fn daemon(&self) -> Remote {
        self.ok(&["daemon", "start"]);
        self.connect().await
    }

    pub async fn connect(&self) -> Remote {
        Remote::connect(&self.socket()).await.expect("connect to the daemon")
    }

    pub fn stop_daemon(&self) {
        self.ok(&["daemon", "stop"]);
        // The daemon removes its socket before draining; its locks go when the process ends.
        wait_until(|| !self.socket().exists(), "the daemon to stop listening");
        wait_until(|| !daemon_lock_held(&self.root), "the daemon to exit");
    }

    /// The "app": an in-process core on its own data dir, mailbox on `relays`.
    pub async fn app(&self, name: &str) -> Client {
        Client::open(app_config(&self.root.join(format!("app-{name}")), &self.relays))
            .await
            .unwrap()
    }
}

pub fn app_config(data_dir: &Path, relays: &[String]) -> NodeConfig {
    NodeConfig {
        retry_interval: TEST_RETRY_INTERVAL,
        mailbox_relays: relays.to_vec(),
        ..NodeConfig::new(data_dir.to_owned())
    }
}

fn tracing_subscriber_init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init()
}

pub fn assert_ok(output: &Output, args: &[&str]) {
    assert!(
        output.status.success(),
        "brumm-cli {args:?} failed ({}):\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn wait_until(mut done: impl FnMut() -> bool, what: &str) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < PATIENCE, "timed out waiting for {what}");
        std::thread::sleep(POLL);
    }
}

fn daemon_lock_held(root: &Path) -> bool {
    let Ok(file) = std::fs::File::open(paths::daemon_lock(root)) else {
        return false;
    };
    file.try_lock_shared().is_err()
}

/// Every check SQLite has: page structure, indexes, free lists.
pub fn assert_db_intact(db: &Path) {
    let connection = rusqlite::Connection::open(db).unwrap();
    let report: Vec<String> = connection
        .prepare("PRAGMA integrity_check")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(report, vec!["ok".to_owned()], "{}", db.display());
    let foreign: i64 = connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(foreign, 0);
}

pub fn db_count(db: &Path, query: &str) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(query, [], |row| row.get(0))
        .unwrap()
}

/// Calls with `profile` set.
pub async fn call(remote: &Remote, profile: &str, method: &str, mut params: Value) -> Value {
    params["profile"] = json!(profile);
    remote
        .call(method, params)
        .await
        .unwrap_or_else(|err| panic!("{method} failed: {err:?}"))
}

/// In-memory Nostr relay; keep the handle alive for the test.
pub async fn local_relay() -> (nostr_sdk::prelude::MockRelay, Vec<String>) {
    let relay = nostr_sdk::prelude::MockRelay::run().await.expect("start local relay");
    let url = relay.url().await.to_string();
    (relay, vec![url])
}

pub fn texts(page: &Value) -> Vec<String> {
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["text"].as_str().unwrap().to_owned())
        .collect()
}
