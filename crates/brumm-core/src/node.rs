//! The node actor: single owner of the store and the transport. It serializes UI commands,
//! inbound peer frames and results of background network tasks into one loop, so state
//! changes never race.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use iroh::SecretKey;
use tokio::sync::mpsc;

use crate::{
    api::{Command, DeliveryStatus, Direction, Event, EventSink, MessageView, ProfileView, Trust},
    clock::now_ms,
    identity::{Profile, UserIdentity, UserKey, fingerprint, normalize_display_name},
    invite::Invite,
    store::{Store, StoredIdentity},
    transport::{InboundRequest, Transport},
    wire::{Body, ChatMessage, Frame, MessageId, RejectReason, Reply},
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

/// Identity + network, available once the user has created or restored an identity.
struct Session {
    user: UserIdentity,
    profile: Profile,
    transport: Arc<Transport>,
}

/// Results of network work spawned off the actor loop.
enum Outcome {
    HelloFinished { invite: Invite, reply: Result<Reply> },
    DeliveryFinished { contact: UserKey, id: MessageId, reply: Result<Reply> },
}

struct Node {
    store: Store,
    events: EventSink,
    session: Option<Session>,
    in_flight: std::collections::HashSet<MessageId>,
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
            session: None,
            in_flight: Default::default(),
            inbound_tx,
            inbound_rx,
            outcomes_tx,
            outcomes_rx,
        })
    }

    async fn start(&mut self) {
        let result = async {
            if let Some(stored) = self.store.load_identity()? {
                let user = UserIdentity::from_phrase(&stored.seed_phrase)?;
                self.start_session(user, stored.profile, stored.device_secret).await?;
            }
            anyhow::Ok(())
        }
        .await;
        self.report(result);
        let contacts = self.store.contacts().unwrap_or_default();
        let profile = self.session.as_ref().map(|session| profile_view(&session.profile));
        self.events.emit(Event::Ready { profile, contacts });
    }

    async fn start_session(&mut self, user: UserIdentity, profile: Profile, device_secret: SecretKey) -> Result<()> {
        let transport = Transport::bind(device_secret, self.inbound_tx.clone()).await?;
        self.session = Some(Session {
            user,
            profile,
            transport: Arc::new(transport),
        });
        Ok(())
    }

    async fn shutdown(self) {
        let Some(session) = self.session else { return };
        drop(session.user);
        match Arc::try_unwrap(session.transport) {
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

    fn session(&self) -> Result<&Session> {
        self.session.as_ref().context("create an identity first")
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
        if self.session.is_some() {
            bail!("an identity already exists on this device");
        }
        let display_name = normalize_display_name(display_name).context("display name is empty or invalid")?;
        let device_secret = SecretKey::generate();
        let profile = Profile {
            display_name,
            device_cert: user.certify_device(device_secret.public(), now_ms()),
        };
        let seed_phrase = user.phrase();
        self.store.save_identity(&StoredIdentity {
            seed_phrase: seed_phrase.clone(),
            device_secret: device_secret.clone(),
            profile: profile.clone(),
        })?;
        let view = profile_view(&profile);
        self.start_session(user, profile, device_secret).await?;
        self.events.emit(Event::IdentityCreated { profile: view, seed_phrase });
        Ok(())
    }

    fn create_invite(&self) -> Result<()> {
        let session = self.session()?;
        let secret: [u8; 16] = rand::random();
        self.store.add_invite(&secret, now_ms())?;
        let transport = session.transport.clone();
        let profile = session.profile.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let invite = Invite::new(profile, transport.addr().await, secret);
            events.emit(Event::InviteCreated { link: invite.to_link() });
        });
        Ok(())
    }

    fn accept_invite(&self, link: &str) -> Result<()> {
        let session = self.session()?;
        let invite = Invite::from_link(link)?;
        if invite.profile.user_key() == session.user.public() {
            bail!("that is your own invite");
        }
        let frame = Frame::Hello {
            invite_secret: invite.secret,
            profile: session.profile.clone(),
        };
        let transport = session.transport.clone();
        let outcomes = self.outcomes_tx.clone();
        tokio::spawn(async move {
            let reply = transport.request(invite.device_addr.clone(), &frame).await;
            let _ = outcomes.send(Outcome::HelloFinished { invite, reply });
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

    /// Spawns delivery to every known device of the contact (one device per contact in M1).
    fn deliver(&mut self, contact: UserKey, message: ChatMessage) -> Result<()> {
        if !self.in_flight.insert(message.id) {
            return Ok(());
        }
        let session = self.session()?;
        let device = *self
            .store
            .devices_of(&contact)?
            .first()
            .context("contact has no known device")?;
        let transport = session.transport.clone();
        let outcomes = self.outcomes_tx.clone();
        tokio::spawn(async move {
            let id = message.id;
            let reply = transport.request(device, &Frame::Message(message)).await;
            let _ = outcomes.send(Outcome::DeliveryFinished { contact, id, reply });
        });
        Ok(())
    }

    fn retry_pending(&mut self) -> Result<()> {
        if self.session.is_none() {
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

    fn finish_hello(&self, invite: Invite, reply: Result<Reply>) -> Result<()> {
        let reply = reply.context("could not reach the inviter — are they online?")?;
        let profile = match reply {
            Reply::HelloAck { profile } => profile,
            Reply::Rejected(RejectReason::UnknownInvite) => bail!("invite was already used or has expired"),
            other => bail!("inviter refused the connection: {other:?}"),
        };
        let matches_invite = profile.user_key() == invite.profile.user_key()
            && profile.is_valid_for(invite.device_addr.id);
        if !matches_invite {
            bail!("inviter answered with a different identity");
        }
        let contact = self.store.upsert_contact(&profile, Trust::Verified, now_ms())?;
        self.events.emit(Event::ContactAdded(contact));
        Ok(())
    }

    // ── inbound frames ──────────────────────────────────────────────────────

    fn handle_inbound(&self, request: InboundRequest) {
        let reply = match self.process_inbound(request.from, request.frame) {
            Ok(reply) => reply,
            Err(err) => {
                tracing::warn!(?err, "inbound frame failed");
                Reply::Rejected(RejectReason::InvalidMessage)
            }
        };
        let _ = request.reply.send(reply);
    }

    fn process_inbound(&self, from: iroh::EndpointId, frame: Frame) -> Result<Reply> {
        let session = self.session()?;
        match frame {
            Frame::Hello { invite_secret, profile } => {
                if !profile.is_valid_for(from) {
                    return Ok(Reply::Rejected(RejectReason::InvalidProfile));
                }
                if !self.store.take_invite(&invite_secret, now_ms())? {
                    return Ok(Reply::Rejected(RejectReason::UnknownInvite));
                }
                let contact = self.store.upsert_contact(&profile, Trust::Verified, now_ms())?;
                self.events.emit(Event::ContactAdded(contact));
                Ok(Reply::HelloAck { profile: session.profile.clone() })
            }
            Frame::Message(message) => {
                let Some(contact) = self.store.contact_for_device(&from)? else {
                    return Ok(Reply::Rejected(RejectReason::UnknownSender));
                };
                if !message.body.is_valid() {
                    return Ok(Reply::Rejected(RejectReason::InvalidMessage));
                }
                if self.store.insert_incoming(&contact, &message, now_ms())? {
                    self.events.emit(Event::MessageAdded(message_view(
                        contact,
                        &message,
                        Direction::Incoming,
                        DeliveryStatus::Delivered,
                    )));
                }
                Ok(Reply::Ack { id: message.id })
            }
        }
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
