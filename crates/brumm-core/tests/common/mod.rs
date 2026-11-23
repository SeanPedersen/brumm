//! Shared harness for end-to-end tests: spawns real nodes and waits for specific events.
#![allow(dead_code)] // each test binary uses a different subset of the harness

use std::{collections::VecDeque, path::PathBuf, time::Duration};

use brumm_core::{
    api::{
        Command, ContactView, ConversationId, DeliveryStatus, DeviceId, DeviceView, Direction,
        Event, EventSink, GroupId, GroupStatus, GroupView, InvitationStage, MessageView,
    },
    identity::UserKey,
    node::{self, GroupTiming, NodeConfig},
    wire::MessageId,
};
use tokio::{sync::mpsc, task::JoinHandle};

pub const EVENT_TIMEOUT: Duration = Duration::from_secs(45);
const TEST_RETRY_INTERVAL: Duration = Duration::from_millis(500);
/// Own devices compare their messages this often in tests (minutes in production).
pub const TEST_RECONCILE_INTERVAL: Duration = Duration::from_secs(1);
const CONVERSATION_POLL: Duration = Duration::from_millis(300);
/// Group timers in tests: rekeys, catch-up and key requests within seconds.
pub const TEST_GROUP_TIMING: GroupTiming = GroupTiming {
    rekey_delay: Duration::from_secs(1),
    catch_up_interval: Duration::from_secs(2),
    key_request_delay: Duration::from_secs(2),
    inline_rekey_bytes: brumm_core::group::keys::INLINE_REKEY_BYTES,
    checkpoint_ops: brumm_core::group::checkpoint::CHECKPOINT_OPS,
    blob_chunk_bytes: 4 * 1024 * 1024,
    inline_log_bytes: brumm_core::group::join::INLINE_LOG_BYTES,
    nostr_log_bytes: brumm_core::group::join::MAX_NOSTR_LOG_BYTES,
};
const DATABASE_FILE: &str = "brumm.db";
/// Nothing listens here: tests without a mailbox relay get no offline delivery.
const NO_RELAY: &str = "ws://127.0.0.1:9";

pub struct TestNode {
    pub data_dir: PathBuf,
    relays: Vec<String>,
    group_timing: GroupTiming,
    pub commands: mpsc::UnboundedSender<Command>,
    pub events: mpsc::UnboundedReceiver<Event>,
    /// Events received but not yet matched by an `expect` (peers deliver in any order).
    buffered: VecDeque<Event>,
    task: JoinHandle<()>,
}

impl TestNode {
    pub fn spawn(name: &str) -> Self {
        Self::spawn_with(name, vec![NO_RELAY.to_owned()])
    }

    /// Node whose mailbox lives on `relays`.
    pub fn spawn_with(name: &str, relays: Vec<String>) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self::spawn_in(
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{unique}")),
            relays,
        )
    }

    /// Node with its own group settings (e.g. rekeys as blobs at any size).
    pub fn spawn_tuned(name: &str, group_timing: GroupTiming) -> Self {
        Self::spawn_tuned_with(name, vec![NO_RELAY.to_owned()], group_timing)
    }

    /// Own group settings and a mailbox on `relays`.
    pub fn spawn_tuned_with(name: &str, relays: Vec<String>, group_timing: GroupTiming) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{unique}"));
        Self::launch(data_dir, relays, group_timing)
    }

    pub fn spawn_in(data_dir: PathBuf, relays: Vec<String>) -> Self {
        Self::launch(data_dir, relays, TEST_GROUP_TIMING)
    }

    fn launch(data_dir: PathBuf, relays: Vec<String>, group_timing: GroupTiming) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let config = NodeConfig {
            retry_interval: TEST_RETRY_INTERVAL,
            mailbox_relays: relays.clone(),
            reconcile_interval: TEST_RECONCILE_INTERVAL,
            group_timing,
            ..NodeConfig::new(data_dir.clone())
        };
        let task = tokio::spawn(node::run(
            config,
            command_rx,
            EventSink::new(event_tx, || {}),
        ));
        Self {
            data_dir,
            relays,
            group_timing,
            commands: command_tx,
            events: event_rx,
            buffered: VecDeque::new(),
            task,
        }
    }

    /// Stops the node (graceful iroh shutdown) and starts a fresh one on the same data dir.
    pub async fn restart(self) -> Self {
        self.stop().await.start()
    }

    /// Stops the node ("goes offline"); keep the result to start it again later.
    pub async fn stop(self) -> Stopped {
        let Self {
            data_dir,
            relays,
            group_timing,
            commands,
            task,
            ..
        } = self;
        drop(commands);
        task.await.expect("node task panicked");
        Stopped {
            data_dir,
            relays,
            group_timing,
        }
    }

    pub fn send(&self, command: Command) {
        self.commands.send(command).expect("node running");
    }

    /// Returns the first event (buffered or new) that `pick` accepts; others stay buffered.
    pub async fn expect<T>(&mut self, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
        if let Some(index) = self.buffered.iter().position(|event| pick(event).is_some()) {
            let event = self.buffered.remove(index).expect("index from position");
            return pick(&event).expect("matched above");
        }
        tokio::time::timeout(EVENT_TIMEOUT, async {
            loop {
                let event = self.events.recv().await.expect("node running");
                if let Event::Error(err) = &event {
                    panic!("node error: {err}");
                }
                match pick(&event) {
                    Some(value) => return value,
                    None => self.buffered.push_back(event),
                }
            }
        })
        .await
        .expect("timed out waiting for event")
    }

    pub async fn expect_error(&mut self) -> String {
        tokio::time::timeout(EVENT_TIMEOUT, async {
            loop {
                if let Event::Error(err) = self.events.recv().await.expect("node running") {
                    return err;
                }
            }
        })
        .await
        .expect("timed out waiting for error")
    }

    /// Sends a text and returns its message id.
    pub async fn send_text(&mut self, contact: UserKey, text: &str) -> MessageId {
        self.send(Command::SendText {
            conversation: ConversationId::Direct(contact),
            text: text.into(),
        });
        let text = text.to_owned();
        self.expect(|event| match event {
            Event::MessageAdded(message)
                if message.direction == Direction::Outgoing && message.text == text =>
            {
                Some(message.id)
            }
            _ => None,
        })
        .await
    }

    pub async fn delivered(&mut self, id: MessageId) {
        self.status(id, DeliveryStatus::Delivered).await
    }

    pub async fn status(&mut self, id: MessageId, status: DeliveryStatus) {
        self.expect(|event| match event {
            Event::MessageStatusChanged {
                id: changed,
                status: new,
                ..
            } if *changed == id && *new == status => Some(()),
            _ => None,
        })
        .await
    }

    /// The whole stored conversation, oldest first.
    pub async fn conversation(&mut self, contact: UserKey) -> Vec<MessageView> {
        let conversation = ConversationId::Direct(contact);
        self.send(Command::LoadConversation {
            conversation,
            limit: None,
        });
        self.expect(|event| match event {
            Event::Conversation {
                conversation: loaded,
                messages,
                ..
            } if *loaded == conversation => Some(messages.clone()),
            _ => None,
        })
        .await
    }

    /// Texts of the conversation once they equal `expected` (in order); panics on timeout.
    pub async fn wait_texts(&mut self, contact: UserKey, expected: &[&str]) {
        let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
        loop {
            let texts = texts(&self.conversation(contact).await);
            if texts == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "conversation stayed at {texts:?}, expected {expected:?}"
            );
            tokio::time::sleep(CONVERSATION_POLL).await;
        }
    }

    pub async fn ready(&mut self) {
        self.expect(|event| matches!(event, Event::Ready { .. }).then_some(()))
            .await;
    }

    /// The chat recorded on [`Event::Ready`] after a restart.
    pub async fn ready_last_opened(&mut self) -> Option<ConversationId> {
        self.expect(|event| match event {
            Event::Ready { last_opened, .. } => Some(*last_opened),
            _ => None,
        })
        .await
    }

    pub async fn create_identity(&mut self, name: &str) -> UserKey {
        self.ready().await;
        self.send(Command::CreateIdentity {
            display_name: name.into(),
        });
        self.expect(|event| match event {
            Event::IdentityCreated { profile, .. } => Some(profile.user_key),
            _ => None,
        })
        .await
    }

    /// Like [`Self::create_identity`], also returning the seed phrase (to sign as this user).
    pub async fn create_identity_with_phrase(&mut self, name: &str) -> (UserKey, String) {
        self.ready().await;
        self.send(Command::CreateIdentity {
            display_name: name.into(),
        });
        self.expect(|event| match event {
            Event::IdentityCreated {
                profile,
                seed_phrase: Some(phrase),
            } => Some((profile.user_key, phrase.clone())),
            _ => None,
        })
        .await
    }

    pub async fn create_invite(&mut self) -> String {
        self.send(Command::CreateInvite);
        self.expect(|event| match event {
            Event::InviteCreated { link } => Some(link.clone()),
            _ => None,
        })
        .await
    }

    pub async fn contact_added(&mut self) -> ContactView {
        self.expect(|event| match event {
            Event::ContactAdded(contact) => Some(contact.clone()),
            _ => None,
        })
        .await
    }

    pub async fn received_text(&mut self) -> String {
        self.message_added(Direction::Incoming).await
    }

    /// Next message added in `direction` (outgoing ones arrive via own-device transcripts).
    pub async fn message_added(&mut self, direction: Direction) -> String {
        self.expect(|event| match event {
            Event::MessageAdded(message) if message.direction == direction => {
                Some(message.text.clone())
            }
            _ => None,
        })
        .await
    }

    /// Links `new_device` (fresh node) to this node's identity; returns the user key and the
    /// new device's id once this node lists `expected_devices` devices.
    pub async fn link(
        &mut self,
        new_device: &mut TestNode,
        expected_devices: usize,
    ) -> (UserKey, DeviceId) {
        new_device.ready().await;
        new_device.send(Command::StartLinking);
        let link = new_device
            .expect(|event| match event {
                Event::LinkTicket { link } => Some(link.clone()),
                _ => None,
            })
            .await;
        self.send(Command::LinkDevice { link });
        let user_key = new_device
            .expect(|event| match event {
                Event::IdentityCreated {
                    profile,
                    seed_phrase: None,
                } => Some(profile.user_key),
                _ => None,
            })
            .await;
        let devices = self.wait_devices(expected_devices).await;
        let new_id = devices.last().expect("at least the new device").device_id;
        (user_key, new_id)
    }

    pub async fn wait_devices(&mut self, count: usize) -> Vec<DeviceView> {
        self.expect(|event| match event {
            Event::Devices(devices) if devices.len() == count => Some(devices.clone()),
            _ => None,
        })
        .await
    }
}

// ── groups ──────────────────────────────────────────────────────────────────

impl TestNode {
    /// Makes `self` and `other` contacts through an invite link.
    pub async fn befriend(&mut self, other: &mut TestNode) {
        let link = self.create_invite().await;
        other.send(Command::AcceptInvite { link });
        other.contact_added().await;
        self.contact_added().await;
    }

    pub async fn group_changed(&mut self, mut pick: impl FnMut(&GroupView) -> bool) -> GroupView {
        self.expect(|event| match event {
            Event::GroupChanged(view) if pick(view) => Some(view.clone()),
            _ => None,
        })
        .await
    }

    /// Creates a group with us as owner, inviting `members`.
    pub async fn create_group(&mut self, name: &str, announcement: bool, members: Vec<UserKey>) -> GroupId {
        self.send(Command::CreateGroup {
            name: name.into(),
            announcement,
            members,
        });
        let name = name.to_owned();
        self.group_changed(|view| view.name == name).await.id
    }

    /// Waits for an invitation to `group` and accepts it; returns once we are a member.
    pub async fn join_group(&mut self, group: GroupId) {
        self.expect(|event| match event {
            Event::GroupInvitations(invitations) => {
                invitations.iter().any(|invitation| invitation.group == group).then_some(())
            }
            _ => None,
        })
        .await;
        self.send(Command::AcceptGroupInvitation { group });
        self.group_changed(|view| view.id == group && view.status == GroupStatus::Active && view.my_role.is_some())
            .await;
    }

    /// Waits until the invitation to `group` reached `stage`.
    pub async fn invitation_stage(&mut self, group: GroupId, stage: InvitationStage) {
        self.expect(|event| match event {
            Event::GroupInvitations(invitations) => invitations
                .iter()
                .any(|invitation| invitation.group == group && invitation.stage == stage)
                .then_some(()),
            _ => None,
        })
        .await;
    }

    /// Waits until the group has `count` members as seen here.
    pub async fn group_members(&mut self, group: GroupId, count: u32) -> GroupView {
        self.group_changed(|view| view.id == group && view.member_count == count).await
    }

    pub async fn send_group_text(&mut self, group: GroupId, text: &str) -> MessageId {
        self.send(Command::SendText {
            conversation: ConversationId::Group(group),
            text: text.into(),
        });
        let text = text.to_owned();
        self.expect(|event| match event {
            Event::MessageAdded(message)
                if message.direction == Direction::Outgoing && message.text == text =>
            {
                Some(message.id)
            }
            _ => None,
        })
        .await
    }

    /// The group's chat messages (no system lines), oldest first.
    pub async fn group_messages(&mut self, group: GroupId) -> Vec<MessageView> {
        let conversation = ConversationId::Group(group);
        self.send(Command::LoadConversation {
            conversation,
            limit: None,
        });
        self.expect(|event| match event {
            Event::Conversation {
                conversation: loaded,
                messages,
                ..
            } if *loaded == conversation => {
                Some(messages.iter().filter(|message| !message.system).cloned().collect())
            }
            _ => None,
        })
        .await
    }

    /// Polls until the group's chat texts equal `expected`.
    pub async fn wait_group_texts(&mut self, group: GroupId, expected: &[&str]) -> Vec<MessageView> {
        let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
        loop {
            let messages = self.group_messages(group).await;
            if texts(&messages) == expected {
                return messages;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "group chat stayed at {:?}, expected {expected:?}",
                texts(&messages)
            );
            tokio::time::sleep(CONVERSATION_POLL).await;
        }
    }

    /// Polls a number from the database until it reaches `at_least`.
    pub async fn wait_sql(&self, query: &str, at_least: i64) {
        let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
        while self.sql_number(query) < at_least {
            assert!(tokio::time::Instant::now() < deadline, "{query} stayed below {at_least}");
            tokio::time::sleep(CONVERSATION_POLL).await;
        }
    }
}

/// A stopped node's data; `start` brings the same device back online.
pub struct Stopped {
    data_dir: PathBuf,
    relays: Vec<String>,
    group_timing: GroupTiming,
}

impl Stopped {
    pub fn start(self) -> TestNode {
        TestNode::launch(self.data_dir, self.relays, self.group_timing)
    }

    /// The stopped node's database, for edits beyond one statement.
    pub fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.data_dir.join(DATABASE_FILE)).expect("open db")
    }

    /// Edits the stopped node's database, e.g. to drop envelopes as if they were lost.
    pub fn sql(&self, statement: &str) -> usize {
        let db = rusqlite::Connection::open(self.data_dir.join(DATABASE_FILE)).expect("open db");
        db.execute(statement, []).expect("statement runs")
    }
}

impl TestNode {
    /// A number from the running node's database (WAL allows a second reader).
    pub fn sql_number(&self, query: &str) -> i64 {
        let db = rusqlite::Connection::open(self.data_dir.join(DATABASE_FILE)).expect("open db");
        db.query_row(query, [], |row| row.get(0))
            .expect("query runs")
    }
}

pub fn texts(messages: &[MessageView]) -> Vec<String> {
    messages
        .iter()
        .map(|message| message.text.clone())
        .collect()
}

/// In-memory Nostr relay for mailbox tests; keep the handle alive for the test's duration.
pub async fn local_relay() -> (nostr_sdk::prelude::MockRelay, Vec<String>) {
    let relay = nostr_sdk::prelude::MockRelay::run()
        .await
        .expect("start local relay");
    let url = relay.url().await.to_string();
    (relay, vec![url])
}
