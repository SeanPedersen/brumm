//! One client connection (a socket client, or the CLI itself in one-shot mode): where its
//! subscriptions' notifications go, and the daemon controls it may use.
//!
//! Backpressure: notifications go through the connection's bounded outbound queue with
//! `try_send`. When it is full, events are dropped and the subscriber gets one `lagged` notice
//! as soon as there is room again; it then re-reads from its cursor. A slow client never makes
//! the core or other clients wait.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use brumm_host::dto::Notice;
use tokio::{
    sync::{broadcast::error::RecvError, mpsc, mpsc::error::TrySendError},
    task::JoinHandle,
};

use super::{Host, InUse};
use crate::protocol::event_line;

/// What only a daemon can do (`daemon.status`, `daemon.stop`).
pub trait DaemonControl: Send + Sync {
    fn status(&self) -> serde_json::Value;
    fn stop(&self);
}

#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

struct Inner {
    host: Arc<Host>,
    outbound: mpsc::Sender<String>,
    daemon: Option<Arc<dyn DaemonControl>>,
    subscriptions: Mutex<HashMap<u64, JoinHandle<()>>>,
    next_subscription: AtomicU64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for (_, forwarder) in self
            .subscriptions
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
        {
            forwarder.abort();
        }
    }
}

impl Session {
    /// `outbound` carries this connection's notification lines.
    pub fn new(
        host: Arc<Host>,
        outbound: mpsc::Sender<String>,
        daemon: Option<Arc<dyn DaemonControl>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                host,
                outbound,
                daemon,
                subscriptions: Mutex::new(HashMap::new()),
                next_subscription: AtomicU64::new(1),
            }),
        }
    }

    pub fn host(&self) -> &Arc<Host> {
        &self.inner.host
    }

    pub fn daemon(&self) -> Option<&Arc<dyn DaemonControl>> {
        self.inner.daemon.as_ref()
    }

    /// Starts pushing `profile`'s events for `rooms` (room ids; all if `None`).
    pub fn subscribe(&self, profile: InUse, rooms: Option<Vec<String>>) -> u64 {
        let id = self.inner.next_subscription.fetch_add(1, Ordering::Relaxed);
        let forwarder = tokio::spawn(forward(id, profile, rooms, self.inner.outbound.clone()));
        self.subscriptions().insert(id, forwarder);
        id
    }

    pub fn unsubscribe(&self, id: u64) -> bool {
        let forwarder = self.subscriptions().remove(&id);
        forwarder.map(|forwarder| forwarder.abort()).is_some()
    }

    fn subscriptions(&self) -> std::sync::MutexGuard<'_, HashMap<u64, JoinHandle<()>>> {
        self.inner
            .subscriptions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Holds the profile in use for as long as the subscription lives (no idle close under it).
/// While lagged, events are dropped until the queue has room for the `lagged` notice, which is
/// then sent at once (not only when a next event comes: the burst may have been the last).
async fn forward(
    id: u64,
    profile: InUse,
    rooms: Option<Vec<String>>,
    outbound: mpsc::Sender<String>,
) {
    let mut events = profile.handle.events();
    let mut lagged = false;
    loop {
        tokio::select! {
            biased;
            permit = outbound.reserve(), if lagged => match permit {
                Ok(permit) => {
                    permit.send(event_line(id, &profile.name, &Notice::Lagged));
                    lagged = false;
                }
                Err(_) => return,
            },
            event = events.recv() => {
                let notice = match event {
                    Ok(event) => match Notice::from_event(&event) {
                        Some(notice) if notice.concerns(rooms.as_deref()) => notice,
                        _ => continue,
                    },
                    Err(RecvError::Lagged(_)) => {
                        lagged = true;
                        continue;
                    }
                    Err(RecvError::Closed) => return,
                };
                if lagged {
                    continue;
                }
                match outbound.try_send(event_line(id, &profile.name, &notice)) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => lagged = true,
                    Err(TrySendError::Closed(_)) => return,
                }
            }
        }
    }
}
