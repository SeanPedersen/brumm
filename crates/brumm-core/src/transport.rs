//! iroh transport: binds the device endpoint, accepts `brumm/1` connections and sends
//! request/response frames to peer devices with a per-peer connection cache.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use anyhow::{Context, Result};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, SecretKey,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use tokio::sync::{mpsc, oneshot};

use crate::wire::{self, ALPN, Frame, MAX_FRAME_BYTES, Reply};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const ONLINE_TIMEOUT: Duration = Duration::from_secs(5);

/// A decoded frame from a peer device awaiting the node's decision.
pub struct InboundRequest {
    pub from: EndpointId,
    pub frame: Frame,
    pub reply: oneshot::Sender<Reply>,
}

pub struct Transport {
    router: Router,
    connections: Mutex<HashMap<EndpointId, Connection>>,
}

impl Transport {
    pub async fn bind(secret_key: SecretKey, inbound: mpsc::Sender<InboundRequest>) -> Result<Self> {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .bind()
            .await
            .context("bind iroh endpoint")?;
        let router = Router::builder(endpoint)
            .accept(ALPN, InboundHandler { inbound })
            .spawn();
        Ok(Self {
            router,
            connections: Mutex::default(),
        })
    }

    fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    /// Our dialable address; waits briefly for a home relay so invites work across NATs.
    pub async fn addr(&self) -> EndpointAddr {
        let _ = tokio::time::timeout(ONLINE_TIMEOUT, self.endpoint().online()).await;
        self.endpoint().addr()
    }

    /// Sends one frame and waits for the reply. Retries once on a stale cached connection.
    pub async fn request(&self, addr: impl Into<EndpointAddr>, frame: &Frame) -> Result<Reply> {
        let addr = addr.into();
        let payload = wire::encode(frame);
        let peer = addr.id;
        if let Some(cached) = self.cached(&peer) {
            match exchange(&cached, &payload).await {
                Ok(reply) => return Ok(reply),
                Err(err) => {
                    tracing::debug!(%peer, ?err, "cached connection failed, redialing");
                    self.forget(&peer);
                }
            }
        }
        let connection = tokio::time::timeout(CONNECT_TIMEOUT, self.endpoint().connect(addr, ALPN))
            .await
            .context("connect timed out")??;
        self.connections.lock().expect("lock poisoned").insert(peer, connection.clone());
        exchange(&connection, &payload).await
    }

    fn cached(&self, peer: &EndpointId) -> Option<Connection> {
        self.connections.lock().expect("lock poisoned").get(peer).cloned()
    }

    fn forget(&self, peer: &EndpointId) {
        self.connections.lock().expect("lock poisoned").remove(peer);
    }

    pub async fn shutdown(self) {
        if let Err(err) = self.router.shutdown().await {
            tracing::warn!(?err, "router shutdown failed");
        }
    }
}

async fn exchange(connection: &Connection, payload: &[u8]) -> Result<Reply> {
    let round_trip = async {
        let (mut send, mut recv) = connection.open_bi().await?;
        send.write_all(payload).await?;
        send.finish()?;
        let response = recv.read_to_end(MAX_FRAME_BYTES).await?;
        anyhow::Ok(wire::decode(&response)?)
    };
    tokio::time::timeout(REQUEST_TIMEOUT, round_trip)
        .await
        .context("request timed out")?
}

#[derive(Debug, Clone)]
struct InboundHandler {
    inbound: mpsc::Sender<InboundRequest>,
}

impl ProtocolHandler for InboundHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let from = connection.remote_id();
        loop {
            let Ok((mut send, mut recv)) = connection.accept_bi().await else {
                return Ok(());
            };
            let inbound = self.inbound.clone();
            tokio::spawn(async move {
                let result = async {
                    let bytes = recv.read_to_end(MAX_FRAME_BYTES).await?;
                    let frame: Frame = wire::decode(&bytes)?;
                    let (reply_tx, reply_rx) = oneshot::channel();
                    inbound
                        .send(InboundRequest { from, frame, reply: reply_tx })
                        .await
                        .context("node stopped")?;
                    let reply = reply_rx.await.context("node dropped request")?;
                    send.write_all(&wire::encode(&reply)).await?;
                    send.finish()?;
                    anyhow::Ok(())
                }
                .await;
                if let Err(err) = result {
                    tracing::debug!(%from, ?err, "inbound stream failed");
                }
            });
        }
    }
}
