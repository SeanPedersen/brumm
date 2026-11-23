//! Where the CLI's requests go: to the daemon over its socket, or (`--no-daemon`) to a host
//! inside this process that runs the same methods and is drained and closed at exit.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

use crate::{
    daemon::OUTBOUND_BUFFER,
    host::{Host, HostConfig, methods, session::Session},
    protocol::RpcError,
    remote::Remote,
};

pub enum Transport {
    Local(Local),
    Remote(Remote),
}

pub struct Local {
    host: Arc<Host>,
    session: Session,
    events: Mutex<mpsc::Receiver<String>>,
}

impl Transport {
    pub fn local(config: HostConfig) -> Self {
        let host = Host::new(config);
        let (outbound, events) = mpsc::channel(OUTBOUND_BUFFER);
        let session = Session::new(host.clone(), outbound, None);
        Self::Local(Local {
            host,
            session,
            events: Mutex::new(events),
        })
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match self {
            Self::Local(local) => methods::call(&local.session, method, params).await,
            Self::Remote(remote) => remote.call(method, params).await,
        }
    }

    /// The `params` of the next subscription notification.
    pub async fn next_event(&self) -> Option<Value> {
        match self {
            Self::Local(local) => {
                let line = local.events.lock().await.recv().await?;
                serde_json::from_str::<Value>(&line)
                    .ok()
                    .map(|mut notification| notification["params"].take())
            }
            Self::Remote(remote) => remote.next_event().await,
        }
    }

    /// One-shot runs drain their profiles to the relays here; the daemon keeps running.
    pub async fn close(self) {
        if let Self::Local(local) = self {
            let Local { host, session, .. } = local;
            drop(session);
            host.shutdown().await;
        }
    }
}
