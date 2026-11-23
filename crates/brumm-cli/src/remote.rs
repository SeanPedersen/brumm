//! Client side of the daemon socket: concurrent calls over one connection (answers matched by
//! `id`), and the `event` notifications of its subscriptions as a stream.

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncWriteExt, BufReader, WriteHalf},
    sync::{mpsc, oneshot},
};

use crate::{
    daemon::socket::{self, ClientStream},
    protocol::{self, EVENT_METHOD, JSONRPC, Response, RpcError, code},
};

/// Notifications not yet taken by [`Remote::next_event`]. When full, further ones are dropped
/// and a `lagged` event is delivered as soon as there is room, so answers to calls never wait
/// behind events and a reader always learns that it missed some.
const EVENT_BUFFER: usize = 1024;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>;

pub struct Remote {
    writer: tokio::sync::Mutex<WriteHalf<ClientStream>>,
    pending: Pending,
    events: tokio::sync::Mutex<mpsc::Receiver<Value>>,
    next_id: AtomicU64,
}

impl Remote {
    pub async fn connect(path: &Path) -> std::io::Result<Self> {
        let stream = socket::connect(path).await?;
        let (read, write) = tokio::io::split(stream);
        let pending: Pending = Arc::default();
        let (event_tx, events) = mpsc::channel(EVENT_BUFFER);
        tokio::spawn(read_loop(BufReader::new(read), pending.clone(), event_tx));
        Ok(Self {
            writer: tokio::sync::Mutex::new(write),
            pending,
            events: tokio::sync::Mutex::new(events),
            next_id: AtomicU64::new(1),
        })
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer_tx, answer) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, answer_tx);
        let mut line = json!({ "jsonrpc": JSONRPC, "id": id, "method": method, "params": params }).to_string();
        line.push('\n');
        let written = self.writer.lock().await.write_all(line.as_bytes()).await;
        if let Err(err) = written {
            self.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            return Err(disconnected(err));
        }
        let response = answer.await.map_err(disconnected)?;
        match (response.result, response.error) {
            (_, Some(error)) => Err(error),
            (Some(result), None) => Ok(result),
            (None, None) => Ok(Value::Null),
        }
    }

    /// The `params` of the next `event` notification (`subscription`, `profile`, `event`);
    /// `None` once the daemon closed the connection.
    pub async fn next_event(&self) -> Option<Value> {
        self.events.lock().await.recv().await
    }
}

fn disconnected(err: impl std::fmt::Display) -> RpcError {
    RpcError::new(code::FAILED, format!("lost the connection to the daemon: {err}"))
}

async fn read_loop(
    mut reader: BufReader<tokio::io::ReadHalf<ClientStream>>,
    pending: Pending,
    events: mpsc::Sender<Value>,
) {
    let mut line = Vec::new();
    let lagged = Arc::new(AtomicBool::new(false));
    while let Ok(Some(())) = protocol::read_line(&mut reader, &mut line).await {
        let Ok(mut message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if message.get("method").and_then(Value::as_str) == Some(EVENT_METHOD) {
            let params = message["params"].take();
            // Dropped while a lagged notice is pending: it tells the reader to read again.
            if lagged.load(Ordering::Acquire) || events.try_send(params.clone()).is_ok() {
                continue;
            }
            lagged.store(true, Ordering::Release);
            let mut notice = params;
            notice["event"] = json!({ "type": "lagged" });
            let (events, lagged) = (events.clone(), lagged.clone());
            tokio::spawn(async move {
                if let Ok(permit) = events.reserve().await {
                    permit.send(notice);
                }
                lagged.store(false, Ordering::Release);
            });
            continue;
        }
        let Ok(response) = serde_json::from_value::<Response>(message) else {
            continue;
        };
        let Some(id) = response.id.as_u64() else { continue };
        let waiter = pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
        if let Some(waiter) = waiter {
            let _ = waiter.send(response);
        }
    }
    // Dropping the waiters fails their calls.
    pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
}
