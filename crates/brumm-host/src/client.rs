//! Async client over the core's `Command`/`Event` channel, for programs without a UI.
//!
//! The core answers commands with uncorrelated events, so every call subscribes to the event
//! stream *before* it sends its command and then waits for the event that answers it (matched by
//! conversation and message id or cursor). The node actor handles commands in order, so a
//! command followed by a `LoadMessage` is done once that lookup is answered: sends look up their
//! own message, other commands a random id ([`Handle::execute`]). A failed command only reports
//! `Event::Error(String)`; with concurrent calls the error text a call returns may belong to
//! another call (and an error from background network work may fail an unrelated call).
//!
//! [`Client`] owns the running core; [`Handle`] (cheap to clone, `Deref` from `Client`) makes the
//! calls, so several tasks can use one core at once.

use std::{
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use brumm_core::{
    api::{
        Command, ContactView, ConversationId, DeliveryStatus, Event, EventSink, GroupId,
        GroupView, MessageView, ProfileView, Role,
    },
    identity::UserKey,
    invite::Invite,
    node::{self, NodeConfig},
    wire::MessageId,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
    time,
};

use crate::{lock::ProfileLock, mirror::Snapshot};

/// Events buffered per subscriber; one that falls further behind misses events (`Lagged`).
const EVENT_BUFFER: usize = 1024;
/// Opening the database and reading the chat list.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Local work only (storage, sealing): answered right away unless the core failed.
pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Needs the network (invites wait for a relay address, accepting for the inviter).
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);
/// No conversation is keyed by this; lookups in it only mark that earlier commands are done.
const BARRIER_CONVERSATION: ConversationId = ConversationId::Direct([0; 32]);

/// Body of a message to send.
#[derive(Debug, Clone)]
pub enum Outgoing {
    Text(String),
    File { name: String, data: Vec<u8> },
}

/// Messages after a read cursor, in arrival order.
#[derive(Debug, Clone)]
pub struct Page {
    pub messages: Vec<MessageView>,
    /// Each message's arrival number (same order): reading on from it continues after it.
    pub arrivals: Vec<u64>,
    /// Pass as `after` to read on from here.
    pub cursor: u64,
    pub has_more: bool,
}

/// A running core for one profile. Dropping it stops the core: it first drains unsent
/// messages to the Nostr relays, and the profile stays locked until it is done.
pub struct Client {
    handle: Handle,
    /// Dropping it stops the core, also while clones of the handle are still around.
    stop: oneshot::Sender<()>,
    node: JoinHandle<()>,
}

/// Makes calls on a running core; clones share it.
#[derive(Clone)]
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<Event>,
    snapshot: Arc<Mutex<Snapshot>>,
    data_dir: PathBuf,
}

impl Deref for Client {
    type Target = Handle;

    fn deref(&self) -> &Handle {
        &self.handle
    }
}

impl Client {
    /// Locks the profile directory (waiting out a previous process's drain) and starts the core
    /// in the current tokio runtime. Returns once the core is ready.
    pub async fn open(config: NodeConfig) -> Result<Self> {
        let data_dir = config.data_dir.clone();
        let lock_dir = data_dir.clone();
        let lock = tokio::task::spawn_blocking(move || ProfileLock::acquire(&lock_dir))
            .await
            .context("profile lock task")??;
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (core_tx, core_rx) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel();
        tokio::spawn(forward_until_stopped(command_rx, core_tx, stopped));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (events, mut ready) = broadcast::channel(EVENT_BUFFER);
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        tokio::spawn(pump(event_rx, events.clone(), snapshot.clone()));
        let node = tokio::spawn(async move {
            // Held until the core is completely done, so a relaunch cannot overlap the drain.
            let _lock = lock;
            node::run(config, core_rx, EventSink::new(event_tx, || {})).await;
        });
        time::timeout(STARTUP_TIMEOUT, async {
            loop {
                match ready.recv().await {
                    Ok(Event::Ready { .. }) => return Ok(()),
                    Ok(Event::Error(error)) => bail!(error),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        bail!("the core stopped while starting")
                    }
                }
            }
        })
        .await
        .map_err(|_| anyhow!("the core did not start within {STARTUP_TIMEOUT:?}"))??;
        Ok(Self {
            handle: Handle {
                commands: command_tx,
                events,
                snapshot,
                data_dir,
            },
            stop,
            node,
        })
    }

    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Stops the core and waits until it is done (drain to the relays, network teardown).
    /// Calls on remaining handle clones fail from then on.
    pub async fn shutdown(self) -> Result<()> {
        let Self { stop, node, .. } = self;
        drop(stop);
        node.await.context("core task failed")
    }
}

impl Handle {
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// What the core reported so far: profile, contacts, groups, invitations.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn profile(&self) -> Option<ProfileView> {
        self.snapshot().profile
    }

    /// The Notes chat (our own key), once an identity exists.
    pub fn notes(&self) -> Option<ConversationId> {
        self.profile()
            .map(|profile| ConversationId::Direct(profile.user_key))
    }

    /// Every event from now on. A subscriber that falls `EVENT_BUFFER` events behind misses some.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Sends a raw command; its answer (if any) arrives on [`Self::events`].
    pub fn command(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow!("the core has stopped"))
    }

    /// Runs a command that has no answer of its own; fails if the core reported an error for it.
    pub async fn execute(&self, command: Command) -> Result<()> {
        self.execute_watching(command, |_| {}).await
    }

    pub async fn create_identity(&self, display_name: &str) -> Result<(ProfileView, String)> {
        self.request(
            Command::CreateIdentity {
                display_name: display_name.to_owned(),
            },
            LOCAL_TIMEOUT,
            |event| match event {
                Event::IdentityCreated {
                    profile,
                    seed_phrase: Some(phrase),
                } => Some(Ok((profile, phrase))),
                Event::Error(error) => Some(Err(anyhow!(error))),
                _ => None,
            },
        )
        .await
    }

    pub async fn restore_identity(&self, phrase: &str, display_name: &str) -> Result<ProfileView> {
        self.request(
            Command::RestoreIdentity {
                phrase: phrase.to_owned(),
                display_name: display_name.to_owned(),
            },
            LOCAL_TIMEOUT,
            |event| match event {
                Event::IdentityCreated { profile, .. } => Some(Ok(profile)),
                Event::Error(error) => Some(Err(anyhow!(error))),
                _ => None,
            },
        )
        .await
    }

    /// A single-use `brumm://invite/` link.
    pub async fn create_invite(&self) -> Result<String> {
        self.request(Command::CreateInvite, NETWORK_TIMEOUT, |event| match event {
            Event::InviteCreated { link } => Some(Ok(link)),
            Event::Error(error) => Some(Err(anyhow!(error))),
            _ => None,
        })
        .await
    }

    /// Accepts an invite link; the inviting device must be online.
    pub async fn accept_invite(&self, link: &str) -> Result<ContactView> {
        let inviter = Invite::from_link(link)
            .context("invalid invite link")?
            .user_key();
        self.request(
            Command::AcceptInvite {
                link: link.to_owned(),
            },
            NETWORK_TIMEOUT,
            |event| match event {
                Event::ContactAdded(contact) if contact.user_key == inviter => Some(Ok(contact)),
                Event::Error(error) => Some(Err(anyhow!(error))),
                _ => None,
            },
        )
        .await
    }

    /// Creates a group with us as owner and invites `members` (contacts).
    pub async fn create_group(
        &self,
        name: &str,
        announcement: bool,
        members: Vec<UserKey>,
    ) -> Result<GroupView> {
        let known: Vec<GroupId> = self.snapshot().groups.iter().map(|group| group.id).collect();
        let name = name.trim().to_owned();
        let mut created = None;
        self.execute_watching(
            Command::CreateGroup {
                name: name.clone(),
                announcement,
                members,
            },
            |event| {
                if let Event::GroupChanged(group) = event
                    && group.name == name
                    && group.my_role == Some(Role::Owner)
                    && !known.contains(&group.id)
                {
                    created = Some(group.clone());
                }
            },
        )
        .await?;
        created.context("the group was not created")
    }

    /// Stores and queues a message. With `id`, a retry of an earlier send (same id) stores
    /// nothing new and still returns `Ok(id)`.
    pub async fn send(
        &self,
        conversation: ConversationId,
        outgoing: Outgoing,
        id: Option<MessageId>,
    ) -> Result<MessageId> {
        let id = id.unwrap_or_else(rand::random);
        let command = match outgoing {
            Outgoing::Text(text) => Command::SendText {
                conversation,
                text,
                id: Some(id),
            },
            Outgoing::File { name, data } => Command::SendFile {
                conversation,
                name,
                data,
                id: Some(id),
            },
        };
        let mut events = self.events.subscribe();
        self.command(command)?;
        self.command(Command::LoadMessage { conversation, id })?;
        let mut error = None;
        await_event(&mut events, LOCAL_TIMEOUT, |event| match event {
            Event::Error(text) => {
                error = Some(text);
                None
            }
            Event::Message {
                conversation: answered,
                id: answered_id,
                message,
            } if answered == conversation && answered_id == id => Some(match message {
                Some(_) => Ok(id),
                None => Err(anyhow!(
                    error
                        .take()
                        .unwrap_or_else(|| "the message was not stored".into())
                )),
            }),
            _ => None,
        })
        .await
    }

    /// The stored message, if any.
    pub async fn message(
        &self,
        conversation: ConversationId,
        id: MessageId,
    ) -> Result<Option<MessageView>> {
        self.request(
            Command::LoadMessage { conversation, id },
            LOCAL_TIMEOUT,
            |event| match event {
                Event::Message {
                    conversation: answered,
                    id: answered_id,
                    message,
                } if answered == conversation && answered_id == id => Some(Ok(message)),
                _ => None,
            },
        )
        .await
    }

    /// Waits until a message reached `target` (Sent or Delivered; Delivered also satisfies
    /// Sent) and returns the status it has. Fails if it failed, was deleted or is unknown.
    pub async fn wait_status(
        &self,
        conversation: ConversationId,
        id: MessageId,
        target: DeliveryStatus,
        timeout: Duration,
    ) -> Result<DeliveryStatus> {
        let mut events = self.events.subscribe();
        self.command(Command::LoadMessage { conversation, id })?;
        let mut best: Option<DeliveryStatus> = None;
        let waited = time::timeout(timeout, async {
            loop {
                let status = match events.recv().await {
                    Ok(Event::Message {
                        conversation: answered,
                        id: answered_id,
                        message,
                    }) if answered == conversation && answered_id == id => {
                        message.context("unknown message")?.status
                    }
                    Ok(Event::MessageStatusChanged {
                        conversation: changed,
                        id: changed_id,
                        status,
                    }) if changed == conversation && changed_id == id => status,
                    Ok(Event::MessagesDeleted {
                        conversation: changed,
                        ids,
                    }) if changed == conversation && ids.contains(&id) => {
                        bail!("the message was deleted")
                    }
                    Ok(_) => continue,
                    // A status change may be among the missed events: ask again.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        self.command(Command::LoadMessage { conversation, id })?;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => bail!("the core has stopped"),
                };
                if status == DeliveryStatus::Failed {
                    bail!("the message could not be delivered");
                }
                if best.is_none_or(|best| progress(status) > progress(best)) {
                    best = Some(status);
                }
                if progress(status) >= progress(target) {
                    return Ok(status);
                }
            }
        })
        .await;
        waited.unwrap_or_else(|_| {
            Err(anyhow!(
                "still {} after {timeout:?}",
                best.map_or("unconfirmed".into(), |status| format!("{status:?}"))
            ))
        })
    }

    /// Up to `limit` messages (all if `None`) that arrived after the cursor `after` (from the
    /// first if `None`), oldest arrival first.
    pub async fn read_since(
        &self,
        conversation: ConversationId,
        after: Option<u64>,
        limit: Option<u32>,
    ) -> Result<Page> {
        self.request(
            Command::LoadNewer {
                conversation,
                after,
                limit,
            },
            LOCAL_TIMEOUT,
            |event| match event {
                Event::NewerMessages {
                    conversation: answered,
                    after: answered_after,
                    messages,
                    arrivals,
                    cursor,
                    has_newer,
                } if answered == conversation && answered_after == after => Some(Ok(Page {
                    messages,
                    arrivals,
                    cursor,
                    has_more: has_newer,
                })),
                _ => None,
            },
        )
        .await
    }

    /// The `limit` messages that arrived last and the cursor after them (to follow from now).
    pub async fn read_latest(&self, conversation: ConversationId, limit: u32) -> Result<Page> {
        self.request(
            Command::LoadLatest {
                conversation,
                limit,
            },
            LOCAL_TIMEOUT,
            |event| match event {
                Event::LatestMessages {
                    conversation: answered,
                    limit: answered_limit,
                    messages,
                    arrivals,
                    cursor,
                } if answered == conversation && answered_limit == limit => Some(Ok(Page {
                    messages,
                    arrivals,
                    cursor,
                    has_more: false,
                })),
                _ => None,
            },
        )
        .await
    }

    /// Sends `command` and returns what `pick` makes of the first event that answers it.
    async fn request<T>(
        &self,
        command: Command,
        timeout: Duration,
        pick: impl FnMut(Event) -> Option<Result<T>>,
    ) -> Result<T> {
        let mut events = self.events.subscribe();
        self.command(command)?;
        await_event(&mut events, timeout, pick).await
    }

    /// Sends `command`, then a lookup that marks the end of its work; `watch` sees every
    /// event in between. The last error in between is the command's error.
    async fn execute_watching(&self, command: Command, mut watch: impl FnMut(&Event)) -> Result<()> {
        let barrier: MessageId = rand::random();
        let mut events = self.events.subscribe();
        self.command(command)?;
        self.command(Command::LoadMessage {
            conversation: BARRIER_CONVERSATION,
            id: barrier,
        })?;
        let mut error = None;
        await_event(&mut events, LOCAL_TIMEOUT, |event| {
            watch(&event);
            match event {
                Event::Error(text) => {
                    error = Some(text);
                    None
                }
                Event::Message { id, .. } if id == barrier => {
                    Some(error.take().map_or(Ok(()), |text| Err(anyhow!(text))))
                }
                _ => None,
            }
        })
        .await
    }
}

/// Waits for the first event `pick` turns into a result.
async fn await_event<T>(
    events: &mut broadcast::Receiver<Event>,
    timeout: Duration,
    mut pick: impl FnMut(Event) -> Option<Result<T>>,
) -> Result<T> {
    time::timeout(timeout, async {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let Some(result) = pick(event) {
                        return result;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    bail!("missed {missed} events from the core (it may have been the answer)")
                }
                Err(broadcast::error::RecvError::Closed) => bail!("the core has stopped"),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("no answer from the core within {timeout:?}"))?
}

/// Passes commands on to the core until the client stops it; then the core's channel closes,
/// which ends the node (after its drain).
async fn forward_until_stopped(
    mut commands: mpsc::UnboundedReceiver<Command>,
    core: mpsc::UnboundedSender<Command>,
    mut stopped: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            _ = &mut stopped => return,
            command = commands.recv() => {
                let Some(command) = command else { return };
                if core.send(command).is_err() {
                    return;
                }
            }
        }
    }
}

/// Forwards core events to every subscriber and keeps the snapshot current.
async fn pump(
    mut core: mpsc::UnboundedReceiver<Event>,
    events: broadcast::Sender<Event>,
    snapshot: Arc<Mutex<Snapshot>>,
) {
    while let Some(event) = core.recv().await {
        snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .apply(&event);
        // No subscriber right now is fine: nobody is waiting for this event.
        let _ = events.send(event);
    }
}

/// How far a message got; `Failed` is handled before this is compared.
fn progress(status: DeliveryStatus) -> u8 {
    match status {
        DeliveryStatus::Pending | DeliveryStatus::Failed => 0,
        DeliveryStatus::Sent => 1,
        DeliveryStatus::Delivered => 2,
    }
}
