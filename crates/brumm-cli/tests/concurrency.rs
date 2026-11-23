//! Concurrency scenarios that could corrupt a profile's SQLite database or lose or duplicate
//! messages: many clients writing at once, retry storms, racing daemon starts, one-shot runs
//! and daemons on the same profile, idle closes racing new requests, and a daemon killed in the
//! middle of a burst of writes. Every test ends with `PRAGMA integrity_check`.

mod common;

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use brumm_cli::{protocol::code, remote::Remote};
use brumm_host::dto;
use common::*;
use serde_json::{Value, json};
use tokio::task::JoinSet;

const CLIENTS: usize = 4;
const SENDS_PER_CLIENT: usize = 25;

fn message_id(client: usize, index: usize) -> String {
    let mut id = [0u8; 16];
    id[0] = client as u8;
    id[1] = index as u8;
    id[15] = 0xbb;
    dto::encode_id(&id)
}

async fn notes_ids(remote: &Remote, profile: &str) -> Vec<String> {
    let page = call(remote, profile, "messages.read", json!({ "room": "notes" })).await;
    page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["id"].as_str().unwrap().to_owned())
        .collect()
}

fn daemon_log(env: &Env) -> String {
    std::fs::read_to_string(brumm_cli::paths::daemon_log(&env.root)).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_sends_and_retry_storms_store_each_message_once() {
    let env = Env::new("conc-sends", vec![NO_RELAY.into()]);
    let first = env.daemon().await;
    call(&first, "p", "identity.init", json!({ "name": "P" })).await;
    let mut connections = Vec::new();
    for _ in 0..CLIENTS {
        connections.push(Arc::new(env.connect().await));
    }
    let mut sends = JoinSet::new();
    for (client, connection) in connections.iter().enumerate() {
        // Each message goes out from its own client and, at the same time, as a retry with the
        // same id from the next client.
        for retry in [false, true] {
            let sender = if retry { connections[(client + 1) % CLIENTS].clone() } else { connection.clone() };
            sends.spawn(async move {
                for index in 0..SENDS_PER_CLIENT {
                    let params = json!({
                        "profile": "p", "room": "notes", "text": format!("{client}/{index}"),
                        "id": message_id(client, index),
                    });
                    sender.call("message.send", params).await.expect("send");
                }
            });
        }
    }
    while let Some(done) = sends.join_next().await {
        done.unwrap();
    }
    let ids = notes_ids(&first, "p").await;
    assert_eq!(ids.len(), CLIENTS * SENDS_PER_CLIENT);
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
    drop((first, connections));
    env.stop_daemon();
    let db = env.profile_db("p");
    assert_db_intact(&db);
    assert_eq!(db_count(&db, "SELECT count(*) FROM messages") as usize, CLIENTS * SENDS_PER_CLIENT);
    // Every stored message got its own arrival number.
    assert_eq!(
        db_count(&db, "SELECT count(DISTINCT seq) FROM messages WHERE seq IS NOT NULL") as usize,
        CLIENTS * SENDS_PER_CLIENT
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn racing_daemon_starts_end_with_one_daemon() {
    const STARTERS: usize = 8;
    let env = Arc::new(Env::new("conc-start", vec![NO_RELAY.into()]));
    let starters: Vec<_> = (0..STARTERS)
        .map(|_| {
            let env = env.clone();
            std::thread::spawn(move || env.json(&["daemon", "start"]).remove(0))
        })
        .collect();
    let pids: HashSet<u64> = starters
        .into_iter()
        .map(|starter| starter.join().unwrap()["pid"].as_u64().unwrap())
        .collect();
    assert_eq!(pids.len(), 1, "several daemons answered: {pids:?}");
    // The losers exited: nothing but the winner holds a lock or a socket.
    std::thread::sleep(Duration::from_millis(500));
    let pid = *pids.iter().next().unwrap();
    let daemons = std::process::Command::new("pgrep")
        .args(["-f", &format!("{}.*daemon run", env.root.display())])
        .output()
        .unwrap();
    let running: Vec<String> = String::from_utf8_lossy(&daemons.stdout).lines().map(str::to_owned).collect();
    assert_eq!(running, vec![pid.to_string()]);
    env.stop_daemon();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_requests_open_a_profile_once() {
    const REQUESTS: usize = 16;
    let env = Env::new("conc-open", vec![NO_RELAY.into()]);
    let remote = Arc::new(env.daemon().await);
    let started = Instant::now();
    let mut requests = JoinSet::new();
    for _ in 0..REQUESTS {
        let remote = remote.clone();
        requests.spawn(async move { remote.call("identity.show", json!({ "profile": "fresh" })).await });
    }
    while let Some(result) = requests.join_next().await {
        result.unwrap().expect("identity.show");
    }
    // A second open of the same profile would wait on its lock (seconds) and then fail.
    assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
    let status = remote.call("daemon.status", json!({})).await.unwrap();
    assert_eq!(status["open"], json!(["fresh"]));
    drop(remote);
    env.stop_daemon();
    assert_eq!(daemon_log(&env).matches("opened").count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_shot_runs_on_one_profile_take_turns() {
    const RUNS: usize = 4;
    let env = Arc::new(Env::new("conc-oneshot", vec![NO_RELAY.into()]));
    env.ok(&["--no-daemon", "-p", "p", "init", "P"]);
    let runs: Vec<_> = (0..RUNS)
        .map(|run| {
            let env = env.clone();
            std::thread::spawn(move || {
                env.run(&["--no-daemon", "-p", "p", "send", "notes", "--wait", "none", &format!("run {run}")])
            })
        })
        .collect();
    for run in runs {
        assert_ok(&run.join().unwrap(), &["parallel one-shot send"]);
    }
    let page = &env.json(&["--no-daemon", "-p", "p", "read", "notes"])[0];
    let mut seen = texts(page);
    seen.sort();
    assert_eq!(seen, (0..RUNS).map(|run| format!("run {run}")).collect::<Vec<_>>());
    assert_db_intact(&env.profile_db("p"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_shot_run_is_refused_while_the_daemon_holds_the_profile() {
    let env = Env::new("conc-busy", vec![NO_RELAY.into()]);
    let remote = env.daemon().await;
    call(&remote, "p", "identity.init", json!({ "name": "P" })).await;
    call(&remote, "p", "message.send", json!({ "room": "notes", "text": "daemon" })).await;
    let output = env.run(&["--no-daemon", "-p", "p", "send", "notes", "one-shot"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("open in another process"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(notes_ids(&remote, "p").await.len(), 1);
    drop(remote);
    env.stop_daemon();
    assert_db_intact(&env.profile_db("p"));
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_closes_racing_new_requests_lose_nothing() {
    const ROUNDS: usize = 12;
    let env = Env::new("conc-idle", vec![NO_RELAY.into()]);
    // The profile closes after 300 ms without use; the daemon stays while we are connected.
    let idle = "300ms";
    env.ok(&["daemon", "start", "--idle-exit", idle]);
    let remote = env.connect().await;
    call(&remote, "p", "identity.init", json!({ "name": "P" })).await;
    for round in 0..ROUNDS {
        // Around the idle boundary: some requests find it open, some closing, some closed.
        tokio::time::sleep(Duration::from_millis(200 + (round as u64 % 4) * 150)).await;
        call(&remote, "p", "message.send", json!({ "room": "notes", "text": format!("round {round}") })).await;
    }
    assert_eq!(notes_ids(&remote, "p").await.len(), ROUNDS);
    drop(remote);
    env.stop_daemon();
    let closes = daemon_log(&env).matches("closed (idle)").count();
    assert!(closes >= 2, "the profile closed {closes} times while idle");
    assert_db_intact(&env.profile_db("p"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_daemon_keeps_every_acknowledged_message() {
    const WRITERS: usize = 4;
    const KILL_AFTER: usize = 40;
    let env = Env::new("conc-kill", vec![NO_RELAY.into()]);
    let remote = env.daemon().await;
    call(&remote, "p", "identity.init", json!({ "name": "P" })).await;
    let pid = remote.call("daemon.status", json!({})).await.unwrap()["pid"].as_u64().unwrap();
    let acknowledged = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let remote = Arc::new(remote);
    let mut writers = JoinSet::new();
    for writer in 0..WRITERS {
        let remote = remote.clone();
        let acknowledged = acknowledged.clone();
        writers.spawn(async move {
            for index in 0.. {
                let id = message_id(writer, index % 256);
                let params = json!({ "profile": "p", "room": "notes", "text": "burst", "id": id });
                match remote.call("message.send", params).await {
                    Ok(result) => acknowledged.lock().unwrap().push(result["id"].as_str().unwrap().to_owned()),
                    Err(_) => return,
                }
            }
        });
    }
    wait_until(|| acknowledged.lock().unwrap().len() >= KILL_AFTER, "a burst of acknowledged sends");
    let killed = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status().unwrap();
    assert!(killed.success());
    while let Some(writer) = writers.join_next().await {
        writer.unwrap();
    }
    drop(remote);
    let acknowledged: HashSet<String> = acknowledged.lock().unwrap().iter().cloned().collect();

    // A fresh daemon (its predecessor left a stale socket behind) opens the same database.
    let remote = env.daemon().await;
    let stored: HashSet<String> = notes_ids(&remote, "p").await.into_iter().collect();
    let lost: Vec<&String> = acknowledged.difference(&stored).collect();
    assert!(lost.is_empty(), "acknowledged but lost: {lost:?}");
    // It still works normally after the crash.
    let after = call(&remote, "p", "message.send", json!({ "room": "notes", "text": "after the crash" })).await;
    assert_eq!(after["status"], "delivered");
    drop(remote);
    env.stop_daemon();
    let db = env.profile_db("p");
    assert_db_intact(&db);
    let distinct = db_count(&db, "SELECT count(DISTINCT seq) FROM messages");
    let total = db_count(&db, "SELECT count(*) FROM messages");
    assert_eq!(distinct, total, "arrival numbers must stay unique after the crash");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_subscriber_is_told_it_lagged_and_others_are_not_held_up() {
    // More than the daemon's outbound queue plus the client's event buffer hold.
    const BURST: usize = brumm_cli::daemon::OUTBOUND_BUFFER + 1024 + 500;
    let env = Env::new("conc-lag", vec![NO_RELAY.into()]);
    let writer = env.daemon().await;
    call(&writer, "p", "identity.init", json!({ "name": "P" })).await;
    let slow = env.connect().await;
    call(&slow, "p", "subscribe", json!({})).await;
    // `slow` never reads its events during the burst; the writer must not be slowed down by it.
    let started = Instant::now();
    for index in 0..BURST {
        call(&writer, "p", "message.send", json!({ "room": "notes", "text": format!("m{index}") })).await;
    }
    assert!(started.elapsed() < Duration::from_secs(60), "the burst took {:?}", started.elapsed());
    let mut lagged = false;
    let mut messages = 0;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), slow.next_event()).await {
        match event["event"]["type"].as_str() {
            Some("lagged") => lagged = true,
            Some("message") => messages += 1,
            _ => {}
        }
    }
    assert!(lagged, "no lagged notice after {messages} of {BURST} messages");
    assert!(messages < BURST);
    // A lagged subscriber catches up from its cursor: nothing is lost there.
    let page = call(&slow, "p", "messages.read", json!({ "room": "notes" })).await;
    assert_eq!(page["messages"].as_array().unwrap().len(), BURST);
    // The slow connection still answers calls.
    let status: Value = slow.call("daemon.status", json!({})).await.unwrap();
    assert_eq!(status["connections"], 2);
    let refused = slow.call("unsubscribe", json!({ "subscription": 999 })).await.unwrap_err();
    assert_eq!(refused.code, code::NOT_FOUND);
    drop((writer, slow));
    env.stop_daemon();
    assert_db_intact(&env.profile_db("p"));
}
