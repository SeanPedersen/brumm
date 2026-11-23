//! iroh transport: binds the device endpoint, accepts `brumm/3` connections and sends
//! request/response frames to peer devices with a per-peer connection cache. The same endpoint
//! runs iroh-gossip for group topics (its own ALPN, SPEC.md §14.4).

use std::{collections::HashMap, sync::Mutex, time::Duration};

use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey, TransportAddr,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use n0_future::{task, time};
use tokio::sync::{OnceCell, mpsc, oneshot};

use crate::{
    group::message::MAX_WIRE_BYTES,
    wire::{self, ALPN, Frame, MAX_FRAME_BYTES, Reply},
};

/// Gossip frames carry one encoded group unit plus iroh-gossip's own framing.
const GOSSIP_FRAMING_BYTES: usize = 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Throughput assumed on top of [`REQUEST_TIMEOUT`] for large frames. A browser always goes
/// through an iroh relay, where a 10 MiB file needs far longer than 10 s.
const MIN_TRANSFER_BYTES_PER_SEC: u64 = 128 * 1024;
/// How long a ticket (invite or link) waits for the home relay. A phone browser on a mobile
/// network can need well over 5 s, and a ticket without the relay is undialable for a browser.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(30);

/// iroh's n0 relays, named without the DNS root dot (`euc1-1.relay.n0.iroh.link`, not
/// `….link.`). WebKit (every iOS browser) fails `fetch` and WebSocket to a host ending in a
/// dot, so a browser could neither probe nor join them. Natives use the same names, because
/// browsers dial a peer's home relay by the URL in its address.
fn relay_map() -> RelayMap {
    iroh::defaults::prod::default_relay_map()
        .relays::<Vec<_>>()
        .into_iter()
        .map(|relay| {
            let mut relay = (*relay).clone();
            relay.url = without_root_dot(&relay.url);
            relay
        })
        .collect()
}

fn without_root_dot(url: &RelayUrl) -> RelayUrl {
    let mut url: url::Url = (**url).clone();
    if let Some(host) = url.host_str().and_then(|host| host.strip_suffix('.'))
        && !host.is_empty()
    {
        let host = host.to_owned();
        let _ = url.set_host(Some(&host));
    }
    RelayUrl::from(url)
}

/// Peers on older versions still advertise the dotted relay names.
fn without_root_dots(addr: EndpointAddr) -> EndpointAddr {
    let addrs: Vec<TransportAddr> = addr
        .addrs
        .into_iter()
        .map(|transport| match transport {
            TransportAddr::Relay(url) => TransportAddr::Relay(without_root_dot(&url)),
            other => other,
        })
        .collect();
    EndpointAddr::from_parts(addr.id, addrs)
}

/// A relay URL, or (natively) at least one direct address. A browser has only its relay.
fn is_dialable(addr: &EndpointAddr) -> bool {
    addr.relay_urls().next().is_some()
        || (!cfg!(target_arch = "wasm32") && addr.ip_addrs().next().is_some())
}

/// A decoded frame from a peer device awaiting the node's decision.
pub struct InboundRequest {
    pub from: EndpointId,
    pub frame: Frame,
    pub reply: oneshot::Sender<Reply>,
}

/// The iroh endpoint, created on first network use. Binding happens in the background so the
/// UI can show the chat list immediately; commands that need the network wait for it here.
pub struct Transport {
    secret_key: SecretKey,
    inbound: mpsc::Sender<InboundRequest>,
    bound: OnceCell<Result<Bound, String>>,
}

struct Bound {
    router: Router,
    gossip: iroh_gossip::Gossip,
    connections: Mutex<HashMap<EndpointId, Connection>>,
}

/// Gossip neighbours a browser keeps per group topic. Everything there goes through one iroh
/// relay connection, so a browser keeps fewer neighbours (and forwards to fewer) than a desktop.
/// 4, not 3: in the 10k-node simulation every message arrived with 4, while 3 lost some (SPEC §14.11).
const BROWSER_ACTIVE_VIEW: usize = 4;

/// HyParView settings: the protocol defaults on desktop, a smaller active view in the browser.
fn gossip_membership() -> iroh_gossip::MembershipConfig {
    let mut config = iroh_gossip::MembershipConfig::default();
    if cfg!(target_family = "wasm") {
        config.active_view_capacity = BROWSER_ACTIVE_VIEW;
    }
    config
}

impl Bound {
    async fn bind(secret_key: SecretKey, inbound: mpsc::Sender<InboundRequest>) -> Result<Self> {
        let endpoint = Endpoint::builder(presets::N0)
            .relay_mode(RelayMode::Custom(relay_map()))
            .secret_key(secret_key)
            .bind()
            .await
            .context("bind iroh endpoint")?;
        let gossip = iroh_gossip::Gossip::builder()
            .max_message_size(MAX_WIRE_BYTES + GOSSIP_FRAMING_BYTES)
            .membership_config(gossip_membership())
            .spawn(endpoint.clone());
        let router = Router::builder(endpoint)
            .accept(ALPN, InboundHandler { inbound })
            .accept(iroh_gossip::ALPN, gossip.clone())
            .spawn();
        Ok(Self {
            router,
            gossip,
            connections: Mutex::default(),
        })
    }

    fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }
}

impl Transport {
    /// Starts binding in the background; callers wait lazily in [`Self::bound`].
    pub fn start(
        secret_key: SecretKey,
        inbound: mpsc::Sender<InboundRequest>,
    ) -> std::sync::Arc<Self> {
        let transport = std::sync::Arc::new(Self {
            secret_key,
            inbound,
            bound: OnceCell::new(),
        });
        let warm = transport.clone();
        task::spawn(async move {
            if let Err(err) = warm.bound().await {
                tracing::warn!(?err, "iroh endpoint unavailable; direct delivery disabled");
            }
        });
        transport
    }

    async fn bound(&self) -> Result<&Bound> {
        let bound = self
            .bound
            .get_or_init(|| async {
                Bound::bind(self.secret_key.clone(), self.inbound.clone())
                    .await
                    .map_err(|err| format!("{err:#}"))
            })
            .await;
        match bound {
            Ok(bound) => Ok(bound),
            Err(message) => bail!("{message}"),
        }
    }

    /// Our dialable address; waits briefly for a home relay so invites work across NATs.
    /// Our address for a ticket. Waits for the home relay; fails instead of returning an
    /// address nobody can dial (only the id), which peers could then only look up via
    /// pkarr/DNS, where a device that never got online has published nothing.
    pub async fn addr(&self) -> Result<EndpointAddr> {
        let bound = time::timeout(ONLINE_TIMEOUT, self.bound())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "the iroh endpoint did not start within {} s — reload the page and try again",
                    ONLINE_TIMEOUT.as_secs()
                )
            })??;
        let online = time::timeout(ONLINE_TIMEOUT, bound.endpoint().online()).await;
        let addr = bound.endpoint().addr();
        if online.is_ok() || is_dialable(&addr) {
            return Ok(addr);
        }
        bail!(
            "could not reach the brumm network (no iroh relay after {} s) — check the connection and try again",
            ONLINE_TIMEOUT.as_secs()
        )
    }

    /// Sends one frame and waits for the reply. Retries once on a stale cached connection.
    pub async fn request(&self, addr: impl Into<EndpointAddr>, frame: &Frame) -> Result<Reply> {
        let bound = self.bound().await?;
        let addr = without_root_dots(addr.into());
        let payload = wire::encode(frame);
        let peer = addr.id;
        if let Some(cached) = bound.cached(&peer) {
            match exchange(&cached, &payload).await {
                Ok(reply) => return Ok(reply),
                Err(err) => {
                    tracing::debug!(%peer, ?err, "cached connection failed, redialing");
                    bound.forget(&peer);
                }
            }
        }
        let connection = time::timeout(CONNECT_TIMEOUT, bound.endpoint().connect(addr, ALPN))
            .await
            .context("connect timed out")??;
        bound
            .connections
            .lock()
            .expect("lock poisoned")
            .insert(peer, connection.clone());
        exchange(&connection, &payload).await
    }

    /// The gossip instance on our endpoint (waits for the endpoint).
    pub async fn gossip(&self) -> Result<iroh_gossip::Gossip> {
        Ok(self.bound().await?.gossip.clone())
    }

    /// Closes the endpoint; in-flight requests of background tasks fail promptly.
    pub async fn shutdown(&self) {
        let Some(Ok(bound)) = self.bound.get() else {
            return;
        };
        if let Err(err) = bound.router.shutdown().await {
            tracing::warn!(?err, "router shutdown failed");
        }
    }
}

impl Bound {
    fn cached(&self, peer: &EndpointId) -> Option<Connection> {
        self.connections
            .lock()
            .expect("lock poisoned")
            .get(peer)
            .cloned()
    }

    fn forget(&self, peer: &EndpointId) {
        self.connections.lock().expect("lock poisoned").remove(peer);
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
    time::timeout(request_timeout(payload.len()), round_trip)
        .await
        .context("request timed out")?
}

/// Base timeout plus the time to move `payload_len` bytes at [`MIN_TRANSFER_BYTES_PER_SEC`].
fn request_timeout(payload_len: usize) -> Duration {
    REQUEST_TIMEOUT + Duration::from_secs(payload_len as u64 / MIN_TRANSFER_BYTES_PER_SEC)
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
            task::spawn(async move {
                let result = async {
                    let bytes = recv.read_to_end(MAX_FRAME_BYTES).await?;
                    let frame: Frame = wire::decode(&bytes)?;
                    let (reply_tx, reply_rx) = oneshot::channel();
                    inbound
                        .send(InboundRequest {
                            from,
                            frame,
                            reply: reply_tx,
                        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_names_have_no_root_dot() {
        let relays: Vec<RelayUrl> = relay_map().urls();
        assert!(!relays.is_empty());
        for url in relays {
            let host = url.host_str().expect("relay host");
            assert!(!host.ends_with('.'), "{url}");
            assert!(host.ends_with("relay.n0.iroh.link"), "{url}");
        }
    }

    #[test]
    fn peer_addresses_lose_the_root_dot() {
        let dotted: RelayUrl = "https://euc1-1.relay.n0.iroh.link./".parse().unwrap();
        let direct: std::net::SocketAddr = "192.0.2.1:4433".parse().unwrap();
        let id = SecretKey::generate().public();
        let addr = without_root_dots(
            EndpointAddr::new(id)
                .with_relay_url(dotted)
                .with_ip_addr(direct),
        );
        let relays: Vec<_> = addr.relay_urls().map(|url| url.to_string()).collect();
        assert_eq!(relays, vec!["https://euc1-1.relay.n0.iroh.link/"]);
        assert_eq!(addr.ip_addrs().copied().collect::<Vec<_>>(), vec![direct]);
    }
}
