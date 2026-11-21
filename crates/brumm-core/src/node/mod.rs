//! The node actor: single owner of the store, the E2EE state and the transport. It
//! serializes UI commands, inbound peer frames and results of background network tasks into
//! one loop, so state changes (including Olm ratchets) never race.
//!
//! Submodules split the behaviour by concern:
//! - [`commands`]: identity creation and UI commands
//! - [`contacts`]: invite handshake (both sides)
//! - [`linking`]: adding / removing devices of our own identity (both sides)
//! - [`delivery`]: per-device outbox, fan-out, acks and the Nostr mailbox fallback
//! - [`inbound`]: decrypting and applying envelopes from peers (direct or via mailbox)
//! - [`reconcile`]: periodic consistency check between own devices (repairs any gap)

mod commands;
mod contacts;
mod delivery;
mod drain;
pub use drain::SHUTDOWN_DRAIN_MAX;
mod history_sync;
mod inbound;
mod linking;
mod logout;
mod reconcile;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use iroh::{EndpointId, SecretKey};
use n0_future::{
    task,
    time::{self, Instant},
};
use tokio::sync::mpsc;
use vodozemac::olm::Session;

use crate::{
    api::{Command, DeviceView, Event, EventSink, ProfileView},
    e2ee::DeviceCrypto,
    identity::{DeviceList, Profile, UserIdentity, UserKey, fingerprint},
    invite::Invite,
    link::LinkTicket,
    mailbox::{self, Mailbox, MailboxDelivery},
    store::Store,
    transport::{InboundRequest, Transport},
    wire::{EnvelopeId, Reply},
};

const DATABASE_FILE: &str = "brumm.db";
const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const RETENTION_MS: u64 = 60 * 24 * 60 * 60 * 1000;
const INBOUND_QUEUE: usize = 64;

pub struct NodeConfig {
    pub data_dir: PathBuf,
    /// How often undelivered envelopes are resent.
    pub retry_interval: Duration,
    /// Nostr relays this device receives offline letters on (published in its device entry).
    pub mailbox_relays: Vec<String>,
    /// Minimum time between two consistency checks with the same own device.
    pub reconcile_interval: Duration,
    /// Called once the shutdown drain is over (everything unsent is on the relays), before the
    /// slower graceful network teardown, so a host can let its process end at that point.
    pub on_drained: Option<Box<dyn FnOnce() + Send>>,
}

impl NodeConfig {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            retry_interval: DEFAULT_RETRY_INTERVAL,
            mailbox_relays: mailbox::default_relays(),
            reconcile_interval: reconcile::DEFAULT_RECONCILE_INTERVAL,
            on_drained: None,
        }
    }
}

/// Runs the node until the command channel closes.
pub async fn run(
    mut config: NodeConfig,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: EventSink,
) {
    let mut node = match Node::open(&config, events.clone()) {
        Ok(node) => node,
        Err(err) => {
            events.emit(Event::Error(format!("failed to open storage: {err:#}")));
            return;
        }
    };
    node.start().await;

    let mut retry = time::interval(config.retry_interval);
    let mut prune = time::interval(PRUNE_INTERVAL);
    let mut reconcile = time::interval(config.reconcile_interval);
    loop {
        let result = tokio::select! {
            command = commands.recv() => match command {
                Some(command) => node.handle_command(command).await,
                None => break,
            },
            Some(request) = node.inbound_rx.recv() => {
                node.handle_inbound(request);
                Ok(())
            }
            Some(outcome) = node.outcomes_rx.recv() => node.handle_outcome(outcome),
            Some(letter) = node.letters_rx.recv() => node.receive_letter(letter),
            _ = retry.tick() => node.flush_outbox(),
            _ = prune.tick() => node.prune(),
            _ = reconcile.tick() => node.reconcile_tick(),
        };
        node.report(result);
        if node.logout_ready() {
            node = match node.finish_logout().await {
                Ok(fresh) => fresh,
                Err(err) => {
                    events.emit(Event::Error(format!("logging out failed: {err:#}")));
                    return;
                }
            };
        }
    }
    node.drain_outbox().await;
    if let Some(on_drained) = config.on_drained.take() {
        on_drained();
    }
    node.shutdown().await;
}

/// Own identity, keys and network; exists once this device belongs to an identity.
struct Me {
    user: UserIdentity,
    /// Own display name and current signed device list.
    profile: Profile,
    device_id: EndpointId,
    crypto: DeviceCrypto,
    transport: Arc<Transport>,
    /// `None` if no relay was reachable at startup (direct delivery still works).
    mailbox: Option<Arc<Mailbox>>,
    revoked: bool,
}

impl Me {
    fn user_key(&self) -> UserKey {
        self.user.public()
    }

    fn own_cert(&self) -> &crate::identity::DeviceCert {
        &self
            .profile
            .devices
            .entry(&self.device_id)
            .expect("own device is always in own list")
            .cert
    }
}

/// A fresh device waiting for an existing device to link it.
struct LinkTarget {
    device_secret: SecretKey,
    crypto: DeviceCrypto,
    transport: Arc<Transport>,
    ticket: LinkTicket,
}

/// A link we initiated, waiting for the new device's ack.
struct PendingLink {
    session: Session,
    devices: DeviceList,
    link_secret: [u8; 16],
}

/// Results of network work spawned off the actor loop.
enum Outcome {
    Hello {
        invite: Box<Invite>,
        reply: Result<Reply>,
    },
    Link {
        device: EndpointId,
        reply: Result<Reply>,
    },
    Delivery {
        device: EndpointId,
        envelope_id: EnvelopeId,
        reply: Result<Reply>,
    },
    Mailboxed {
        device: EndpointId,
        envelope_id: EnvelopeId,
        result: Result<()>,
    },
    /// Reply to an envelope sent without the outbox ([`Node::send_direct`]).
    Direct {
        device: EndpointId,
        sealed_at_ms: u64,
        reply: Result<Reply>,
    },
    MailboxReady(Arc<Mailbox>),
    /// The log-out grace period ended; erase now even if some announcements are still pending.
    LogoutDeadline,
}

struct Node {
    store: Store,
    events: EventSink,
    mailbox_relays: Vec<String>,
    me: Option<Me>,
    link_target: Option<LinkTarget>,
    /// Outbound sessions created from invites, waiting for the inviter's `HelloAck`.
    pending_hellos: HashMap<EndpointId, Session>,
    pending_links: HashMap<EndpointId, PendingLink>,
    in_flight: HashSet<(EndpointId, EnvelopeId)>,
    /// Envelopes currently being posted to a mailbox.
    mailboxing: HashSet<(EndpointId, EnvelopeId)>,
    /// When we last restarted the session to a device that could not decrypt (Unix ms).
    fresh_sessions: HashMap<EndpointId, u64>,
    reconcile: reconcile::ReconcileState,
    reconcile_interval: Duration,
    inbound_tx: mpsc::Sender<InboundRequest>,
    inbound_rx: mpsc::Receiver<InboundRequest>,
    outcomes_tx: mpsc::UnboundedSender<Outcome>,
    outcomes_rx: mpsc::UnboundedReceiver<Outcome>,
    letters_tx: mpsc::Sender<MailboxDelivery>,
    letters_rx: mpsc::Receiver<MailboxDelivery>,
    /// Set by `Command::LogOut`: the node only finishes network work, then erases itself.
    logging_out: bool,
    logout_deadline_passed: bool,
}

impl Node {
    fn open(config: &NodeConfig, events: EventSink) -> Result<Self> {
        #[cfg(not(target_family = "wasm"))]
        std::fs::create_dir_all(&config.data_dir)?;
        let store = Store::open(&config.data_dir.join(DATABASE_FILE))?;
        Ok(Self::with_store(
            store,
            events,
            config.mailbox_relays.clone(),
            config.reconcile_interval,
        ))
    }

    /// Fresh in-memory state over an opened store. New channels, so results of network tasks
    /// spawned by an earlier node (before a log-out) are dropped.
    fn with_store(
        store: Store,
        events: EventSink,
        mailbox_relays: Vec<String>,
        reconcile_interval: Duration,
    ) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_QUEUE);
        let (outcomes_tx, outcomes_rx) = mpsc::unbounded_channel();
        let (letters_tx, letters_rx) = mpsc::channel(INBOUND_QUEUE);
        Self {
            store,
            events,
            mailbox_relays,
            me: None,
            link_target: None,
            pending_hellos: HashMap::new(),
            pending_links: HashMap::new(),
            in_flight: HashSet::new(),
            mailboxing: HashSet::new(),
            fresh_sessions: HashMap::new(),
            reconcile: reconcile::ReconcileState::default(),
            reconcile_interval,
            inbound_tx,
            inbound_rx,
            outcomes_tx,
            outcomes_rx,
            letters_tx,
            letters_rx,
            logging_out: false,
            logout_deadline_passed: false,
        }
    }

    /// Connects to our mailbox relays in the background; reports back via `Outcome::MailboxReady`.
    /// Failure is logged, not fatal: direct delivery still works.
    fn start_mailbox(&self, device_secret: &SecretKey, relays: Vec<String>) {
        let since = self.store.mailbox_since().ok().flatten();
        let secret = device_secret.to_bytes();
        let letters = self.letters_tx.clone();
        let outcomes = self.outcomes_tx.clone();
        task::spawn(async move {
            match Mailbox::start(&secret, &relays, since, letters).await {
                Ok(mailbox) => {
                    let _ = outcomes.send(Outcome::MailboxReady(mailbox));
                }
                Err(err) => tracing::warn!(
                    ?err,
                    "mailbox unavailable; offline delivery disabled for this session"
                ),
            }
        });
    }

    async fn start(&mut self) {
        let started = Instant::now();
        let loaded = self.load_me().await;
        self.report(loaded);
        let contacts = self.store.contacts().unwrap_or_default();
        let previews = self.store.conversation_previews().unwrap_or_default();
        let profile = self.me.as_ref().map(|me| profile_view(&me.profile));
        // One primary-key read. The conversation itself loads later, after Ready.
        let last_opened = self.store.last_opened().unwrap_or(None);
        self.events.emit(Event::Ready {
            profile,
            contacts,
            previews,
            last_opened,
        });
        for (user, image) in self.store.avatars().unwrap_or_default() {
            self.emit_avatar(&user, &image);
        }
        self.emit_devices();
        let syncing = self.emit_history_sync();
        self.report(syncing);
        if self.me.as_ref().is_some_and(|me| me.revoked) {
            self.events.emit(Event::DeviceRevoked);
        }
        tracing::info!(elapsed_ms = started.elapsed().as_millis(), "storage ready");
        let flushed = self.flush_outbox();
        self.report(flushed);
    }

    async fn load_me(&mut self) -> Result<()> {
        let Some(stored) = self.store.load_identity()? else {
            return Ok(());
        };
        let revoked = self.store.is_revoked()?;
        let user = UserIdentity::from_phrase(&stored.seed_phrase)?;
        let crypto = DeviceCrypto::load(&stored.olm_account, &stored.device_secret.to_bytes())?;
        let transport = Transport::start(stored.device_secret.clone(), self.inbound_tx.clone());
        let device_id = stored.device_secret.public();
        let relays = stored
            .profile
            .devices
            .entry(&device_id)
            .map(|entry| entry.mailbox.relays.clone())
            .unwrap_or_else(|| self.mailbox_relays.clone());
        if !revoked {
            self.start_mailbox(&stored.device_secret, relays);
        }
        self.me = Some(Me {
            user,
            profile: stored.profile,
            device_id,
            crypto,
            transport,
            mailbox: None,
            revoked,
        });
        Ok(())
    }

    async fn shutdown(self) {
        shutdown_network(self.me, self.link_target).await;
    }

    fn report(&self, result: Result<()>) {
        if let Err(err) = result {
            tracing::warn!(?err, "node error");
            self.events.emit(Event::Error(format!("{err:#}")));
        }
    }

    fn me(&self) -> Result<&Me> {
        self.me.as_ref().context("create an identity first")
    }

    /// Identity that is allowed to act (not revoked).
    fn active_me(&self) -> Result<&Me> {
        let me = self.me()?;
        anyhow::ensure!(!me.revoked, "this device was removed from your account");
        Ok(me)
    }

    fn emit_devices(&self) {
        let Some(me) = &self.me else { return };
        let devices = me
            .profile
            .devices
            .entries
            .iter()
            .map(|entry| DeviceView {
                device_id: *entry.device_id().as_bytes(),
                label: entry.label.clone(),
                added_at_ms: entry.cert.issued_at_ms,
                this_device: entry.device_id() == me.device_id,
            })
            .collect();
        self.events.emit(Event::Devices(devices));
    }

    fn prune(&self) -> Result<()> {
        let Some(me) = &self.me else { return Ok(()) };
        let removed = self.store.prune_messages_before(
            crate::clock::now_ms().saturating_sub(RETENTION_MS),
            &me.user_key(),
        )?;
        if removed > 0 {
            tracing::info!(removed, "pruned messages past retention");
        }
        Ok(())
    }

    fn handle_outcome(&mut self, outcome: Outcome) -> Result<()> {
        match outcome {
            Outcome::Hello { invite, reply } => self.finish_hello(*invite, reply),
            Outcome::Link { device, reply } => self.finish_link(device, reply),
            Outcome::Delivery {
                device,
                envelope_id,
                reply,
            } => self.finish_delivery(device, envelope_id, reply),
            Outcome::Mailboxed {
                device,
                envelope_id,
                result,
            } => self.finish_mailboxing(device, envelope_id, result),
            Outcome::Direct {
                device,
                sealed_at_ms,
                reply,
            } => self.finish_direct(device, sealed_at_ms, reply),
            Outcome::MailboxReady(mailbox) => {
                if let Some(me) = self.me.as_mut() {
                    me.mailbox = Some(mailbox);
                }
                Ok(())
            }
            Outcome::LogoutDeadline => {
                self.logout_deadline_passed = true;
                Ok(())
            }
        }
    }
}

async fn shutdown_network(me: Option<Me>, link_target: Option<LinkTarget>) {
    if let Some(mailbox) = me.as_ref().and_then(|me| me.mailbox.as_ref()) {
        mailbox.shutdown().await;
    }
    let transport = match (me, link_target) {
        (Some(me), _) => me.transport,
        (None, Some(target)) => target.transport,
        (None, None) => return,
    };
    transport.shutdown().await;
}

/// Next device-list sequence number: strictly increasing, and time-based so that a device
/// restored from the seed phrase supersedes lists it never saw.
fn next_seq(current: u64) -> u64 {
    (current + 1).max(crate::clock::now_ms())
}

fn profile_view(profile: &Profile) -> ProfileView {
    let user_key = profile.user_key();
    ProfileView {
        user_key,
        display_name: profile.display_name.clone(),
        fingerprint: fingerprint(&user_key),
    }
}
