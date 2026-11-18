//! The node actor: single owner of the store, the E2EE state and the transport. It
//! serializes UI commands, inbound peer frames and results of background network tasks into
//! one loop, so state changes (including Olm ratchets) never race.

use std::{collections::{HashMap, HashSet}, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, SecretKey};
use tokio::sync::mpsc;
use vodozemac::olm::Session;

use crate::{
    api::{Command, DeliveryStatus, Direction, Event, EventSink, MessageView, ProfileView, Trust},
    clock::now_ms,
    crypto::{self, Sealed},
    e2ee::DeviceCrypto,
    identity::{Profile, UserIdentity, UserKey, fingerprint, normalize_display_name},
    invite::Invite,
    store::{Store, StoredIdentity},
    transport::{InboundRequest, Transport},
    wire::{self, Body, ChatMessage, Frame, MessageId, Payload, RejectReason, Reply},
};

const DATABASE_FILE: &str = "brumm.db";
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const PRUNE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const RETENTION_MS: u64 = 60 * 24 * 60 * 60 * 1000;
const INBOUND_QUEUE: usize = 64;

pub struct NodeConfig {
    pub data_dir: PathBuf,
}

/// Runs the node until the command channel closes.
pub async fn run(config: NodeConfig, mut commands: mpsc::UnboundedReceiver<Command>, events: EventSink) {
    let mut node = match Node::open(&config, events.clone()) {
        Ok(node) => node,
        Err(err) => {
            events.emit(Event::Error(format!("failed to open storage: {err:#}")));
            return;
        }
    };
    node.start().await;

    let mut retry = tokio::time::interval(RETRY_INTERVAL);
    let mut prune = tokio::time::interval(PRUNE_INTERVAL);
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
            _ = retry.tick() => node.retry_pending(),
            _ = prune.tick() => node.prune(),
        };
        node.report(result);
    }
    node.shutdown().await;
}

/// Own identity, keys and network; exists once the user created or restored an identity.
struct Me {
    user: UserIdentity,
    profile: Profile,
    crypto: DeviceCrypto,
    transport: Arc<Transport>,
}

/// Results of network work spawned off the actor loop.
enum Outcome {
    HelloFinished { invite: Box<Invite>, reply: Result<Reply> },
    DeliveryFinished { contact: UserKey, id: MessageId, reply: Result<Reply> },
}

struct Node {
    store: Store,
    events: EventSink,
    me: Option<Me>,
    /// Outbound sessions created from invites, waiting for the inviter's `HelloAck`.
    pending_hellos: HashMap<EndpointId, Session>,
    in_flight: HashSet<MessageId>,
    inbound_tx: mpsc::Sender<InboundRequest>,
    inbound_rx: mpsc::Receiver<InboundRequest>,
    outcomes_tx: mpsc::UnboundedSender<Outcome>,
    outcomes_rx: mpsc::UnboundedReceiver<Outcome>,
}

impl Node {
    fn open(config: &NodeConfig, events: EventSink) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        let store = Store::open(&config.data_dir.join(DATABASE_FILE))?;
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_QUEUE);
        let (outcomes_tx, outcomes_rx) = mpsc::unbounded_channel();
        Ok(Self {
            store,
            events,
            me: None,
            pending_hellos: HashMap::new(),
            in_flight: HashSet::new(),
            inbound_tx,
            inbound_rx,
            outcomes_tx,
            outcomes_rx,
        })
    }

    async fn start(&mut self) {
        let result = async {
            let Some(stored) = self.store.load_identity()? else {
                return anyhow::Ok(());
            };
            let user = UserIdentity::from_phrase(&stored.seed_phrase)?;
            let crypto = DeviceCrypto::load(&stored.olm_account, &stored.device_secret.to_bytes())?;
            self.go_online(user, stored.profile, crypto, stored.device_secret).await
        }
        .await;
        self.report(result);
        let contacts = self.store.contacts().unwrap_or_default();
        let profile = self.me.as_ref().map(|me| profile_view(&me.profile));
        self.events.emit(Event::Ready { profile, contacts });
    }

    async fn go_online(&mut self, user: UserIdentity, profile: Profile, crypto: DeviceCrypto, device_secret: SecretKey) -> Result<()> {
        let transport = Transport::bind(device_secret, self.inbound_tx.clone()).await?;
        self.me = Some(Me {
            user,
            profile,
            crypto,
            transport: Arc::new(transport),
        });
        Ok(())
    }

    async fn shutdown(self) {
        let Some(me) = self.me else { return };
        match Arc::try_unwrap(me.transport) {
            Ok(transport) => transport.shutdown().await,
            Err(_) => tracing::debug!("transport still in use by a background task at shutdown"),
        }
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

    // ── commands ────────────────────────────────────────────────────────────

    async fn handle_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::CreateIdentity { display_name } => {
                self.create_identity(UserIdentity::generate(), &display_name).await
            }
            Command::RestoreIdentity { phrase, display_name } => {
                let user = UserIdentity::from_phrase(&phrase).context("invalid seed phrase")?;
                self.create_identity(user, &display_name).await
            }
            Command::CreateInvite => self.create_invite(),
            Command::AcceptInvite { link } => self.accept_invite(&link),
            Command::LoadConversation { contact } => {
                let messages = self.store.conversation(&contact)?;
                self.events.emit(Event::Conversation { contact, messages });
                Ok(())
            }
            Command::SendText { contact, text } => self.send_text(contact, text),
        }
    }

    async fn create_identity(&mut self, user: UserIdentity, display_name: &str) -> Result<()> {
        if self.me.is_some() {
            bail!("an identity already exists on this device");
        }
        let display_name = normalize_display_name(display_name).context("display name is empty or invalid")?;
        let device_secret = SecretKey::generate();
        let crypto = DeviceCrypto::generate(&device_secret.to_bytes());
        let profile = Profile {
            display_name,
            device_cert: user.certify_device(device_secret.public(), crypto.identity_key(), now_ms()),
        };
        let seed_phrase = user.phrase();
        self.store.save_identity(&StoredIdentity {
            seed_phrase: seed_phrase.clone(),
            device_secret: device_secret.clone(),
            profile: profile.clone(),
            olm_account: crypto.account_pickle(),
        })?;
        let view = profile_view(&profile);
        self.go_online(user, profile, crypto, device_secret).await?;
        self.events.emit(Event::IdentityCreated { profile: view, seed_phrase });
        Ok(())
    }

    fn create_invite(&mut self) -> Result<()> {
        let secret: [u8; 16] = rand::random();
        let Self { store, me, events, .. } = self;
        let me = me.as_mut().context("create an identity first")?;
        let one_time_key = me.crypto.new_one_time_key(store)?;
        store.add_invite(&secret, now_ms())?;
        let transport = me.transport.clone();
        let profile = me.profile.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let invite = Invite::new(profile, transport.addr().await, secret, one_time_key);
            events.emit(Event::InviteCreated { link: invite.to_link() });
        });
        Ok(())
    }

    fn accept_invite(&mut self, link: &str) -> Result<()> {
        let invite = Invite::from_link(link)?;
        let me = self.me()?;
        if invite.profile.user_key() == me.user.public() {
            bail!("that is your own invite");
        }
        let mut session = me
            .crypto
            .start_outbound(&invite.profile.device_cert.olm_identity_key, &invite.one_time_key)?;
        let frame = Frame::Hello {
            invite_secret: invite.secret,
            sealed: crypto::seal(&mut session, &wire::encode(&me.profile))?,
        };
        let transport = me.transport.clone();
        self.pending_hellos.insert(invite.device_addr.id, session);
        let outcomes = self.outcomes_tx.clone();
        tokio::spawn(async move {
            let reply = transport.request(invite.device_addr.clone(), &frame).await;
            let _ = outcomes.send(Outcome::HelloFinished { invite: Box::new(invite), reply });
        });
        Ok(())
    }

    fn send_text(&mut self, contact: UserKey, text: String) -> Result<()> {
        self.store.contact(&contact)?.context("unknown contact")?;
        let message = ChatMessage {
            id: rand::random(),
            sent_at_ms: now_ms(),
            body: Body::Text(text.trim().to_owned()),
        };
        if !message.body.is_valid() {
            bail!("message is empty or too long");
        }
        self.store.insert_outgoing(&contact, &message)?;
        self.events.emit(Event::MessageAdded(message_view(
            contact,
            &message,
            Direction::Outgoing,
            DeliveryStatus::Pending,
        )));
        self.deliver(contact, message)
    }

    /// Encrypts for and spawns delivery to the contact's device (one device per contact until
    /// multi-device). Re-encrypts on every attempt; receivers deduplicate by message id.
    fn deliver(&mut self, contact: UserKey, message: ChatMessage) -> Result<()> {
        if self.in_flight.contains(&message.id) {
            return Ok(());
        }
        let device = *self
            .store
            .devices_of(&contact)?
            .first()
            .context("contact has no known device")?;
        let Self { store, me, .. } = self;
        let me = me.as_mut().context("create an identity first")?;
        let sealed = me.crypto.seal(store, &device, &wire::encode(&Payload::Chat(message.clone())))?;
        let transport = me.transport.clone();
        let outcomes = self.outcomes_tx.clone();
        let id = message.id;
        self.in_flight.insert(id);
        tokio::spawn(async move {
            let reply = transport.request(device, &Frame::Sealed(sealed)).await;
            let _ = outcomes.send(Outcome::DeliveryFinished { contact, id, reply });
        });
        Ok(())
    }

    fn retry_pending(&mut self) -> Result<()> {
        if self.me.is_none() {
            return Ok(());
        }
        for (contact, message) in self.store.pending_outgoing()? {
            self.deliver(contact, message)?;
        }
        Ok(())
    }

    fn prune(&self) -> Result<()> {
        let removed = self.store.prune_messages_before(now_ms().saturating_sub(RETENTION_MS))?;
        if removed > 0 {
            tracing::info!(removed, "pruned messages past retention");
        }
        Ok(())
    }

    // ── network outcomes ────────────────────────────────────────────────────

    fn handle_outcome(&mut self, outcome: Outcome) -> Result<()> {
        match outcome {
            Outcome::HelloFinished { invite, reply } => self.finish_hello(invite, reply),
            Outcome::DeliveryFinished { contact, id, reply } => {
                self.in_flight.remove(&id);
                let status = match reply {
                    Ok(Reply::Ack { id: acked }) if acked == id => DeliveryStatus::Delivered,
                    Ok(Reply::Rejected(reason)) => {
                        tracing::warn!(?reason, "message rejected by peer");
                        DeliveryStatus::Failed
                    }
                    Ok(other) => {
                        tracing::warn!(?other, "unexpected reply to message");
                        return Ok(());
                    }
                    Err(err) => {
                        tracing::debug!(?err, "delivery failed, will retry");
                        return Ok(());
                    }
                };
                self.store.set_status(&contact, &id, status)?;
                self.events.emit(Event::MessageStatusChanged { contact, id, status });
                Ok(())
            }
        }
    }

    fn finish_hello(&mut self, invite: Box<Invite>, reply: Result<Reply>) -> Result<()> {
        let device = invite.device_addr.id;
        let mut session = self.pending_hellos.remove(&device).context("no pending invite for this device")?;
        let reply = reply.context("could not reach the inviter — are they online?")?;
        let sealed = match reply {
            Reply::HelloAck { sealed } => sealed,
            Reply::Rejected(RejectReason::UnknownInvite) => bail!("invite was already used or has expired"),
            other => bail!("inviter refused the connection: {other:?}"),
        };
        let profile: Profile = wire::decode(&crypto::open(&mut session, &sealed)?)?;
        let matches_invite = profile.user_key() == invite.profile.user_key()
            && profile.device_cert.olm_identity_key == invite.profile.device_cert.olm_identity_key
            && profile.is_valid_for(device);
        if !matches_invite {
            bail!("inviter answered with a different identity");
        }
        let Self { store, me, events, .. } = self;
        let me = me.as_mut().context("no identity")?;
        let contact = store.upsert_contact(&profile, Trust::Verified, now_ms())?;
        me.crypto.adopt(store, device, session)?;
        events.emit(Event::ContactAdded(contact));
        Ok(())
    }

    // ── inbound frames ──────────────────────────────────────────────────────

    fn handle_inbound(&mut self, request: InboundRequest) {
        let reply = match self.process_inbound(request.from, request.frame) {
            Ok(reply) => reply,
            Err(err) => {
                tracing::warn!(?err, "inbound frame failed");
                Reply::Rejected(RejectReason::InvalidMessage)
            }
        };
        let _ = request.reply.send(reply);
    }

    fn process_inbound(&mut self, from: EndpointId, frame: Frame) -> Result<Reply> {
        match frame {
            Frame::Hello { invite_secret, sealed } => self.accept_hello(from, invite_secret, &sealed),
            Frame::Sealed(sealed) => self.receive_sealed(from, &sealed),
        }
    }

    fn accept_hello(&mut self, from: EndpointId, invite_secret: wire::InviteSecret, sealed: &Sealed) -> Result<Reply> {
        let Self { store, me, events, .. } = self;
        let me = me.as_mut().context("no identity")?;
        if sealed.pre_key_sender().is_none() {
            return Ok(Reply::Rejected(RejectReason::InvalidProfile));
        }
        if !store.take_invite(&invite_secret, now_ms())? {
            return Ok(Reply::Rejected(RejectReason::UnknownInvite));
        }
        let (mut session, plaintext, sender_identity) = me.crypto.accept_inbound(store, sealed)?;
        let profile: Profile = wire::decode(&plaintext)?;
        if !profile.is_valid_for(from) || profile.device_cert.olm_identity_key != sender_identity {
            return Ok(Reply::Rejected(RejectReason::InvalidProfile));
        }
        let contact = store.upsert_contact(&profile, Trust::Verified, now_ms())?;
        let ack = crypto::seal(&mut session, &wire::encode(&me.profile))?;
        me.crypto.adopt(store, from, session)?;
        events.emit(Event::ContactAdded(contact));
        Ok(Reply::HelloAck { sealed: ack })
    }

    fn receive_sealed(&mut self, from: EndpointId, sealed: &Sealed) -> Result<Reply> {
        let Self { store, me, events, .. } = self;
        let me = me.as_mut().context("no identity")?;
        let (Some(contact), Some(cert)) = (store.contact_for_device(&from)?, store.device_cert(&from)?) else {
            return Ok(Reply::Rejected(RejectReason::UnknownSender));
        };
        let plaintext = me.crypto.open(store, &from, &cert.olm_identity_key, sealed)?;
        let Payload::Chat(message) = wire::decode(&plaintext)?;
        if !message.body.is_valid() {
            return Ok(Reply::Rejected(RejectReason::InvalidMessage));
        }
        if store.insert_incoming(&contact, &message, now_ms())? {
            events.emit(Event::MessageAdded(message_view(
                contact,
                &message,
                Direction::Incoming,
                DeliveryStatus::Delivered,
            )));
        }
        Ok(Reply::Ack { id: message.id })
    }
}

fn profile_view(profile: &Profile) -> ProfileView {
    let user_key = profile.user_key();
    ProfileView {
        user_key,
        display_name: profile.display_name.clone(),
        fingerprint: fingerprint(&user_key),
    }
}

fn message_view(contact: UserKey, message: &ChatMessage, direction: Direction, status: DeliveryStatus) -> MessageView {
    let Body::Text(text) = &message.body;
    MessageView {
        id: message.id,
        contact,
        direction,
        text: text.clone(),
        sent_at_ms: message.sent_at_ms,
        status,
    }
}
