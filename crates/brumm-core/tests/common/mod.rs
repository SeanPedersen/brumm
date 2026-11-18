//! Shared harness for end-to-end tests: spawns real nodes and waits for specific events.
#![allow(dead_code)] // each test binary uses a different subset of the harness

use std::{path::PathBuf, time::Duration};

use brumm_core::{
    api::{Command, ContactView, Event, EventSink},
    identity::UserKey,
    node::{self, NodeConfig},
};
use tokio::{sync::mpsc, task::JoinHandle};

pub const EVENT_TIMEOUT: Duration = Duration::from_secs(45);

pub struct TestNode {
    pub data_dir: PathBuf,
    pub commands: mpsc::UnboundedSender<Command>,
    pub events: mpsc::UnboundedReceiver<Event>,
    task: JoinHandle<()>,
}

impl TestNode {
    pub fn spawn(name: &str) -> Self {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        Self::spawn_in(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{unique}")))
    }

    pub fn spawn_in(data_dir: PathBuf) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let config = NodeConfig { data_dir: data_dir.clone() };
        let task = tokio::spawn(node::run(config, command_rx, EventSink::new(event_tx, || {})));
        Self { data_dir, commands: command_tx, events: event_rx, task }
    }

    /// Stops the node (graceful iroh shutdown) and starts a fresh one on the same data dir.
    pub async fn restart(self) -> Self {
        let Self { data_dir, commands, task, .. } = self;
        drop(commands);
        task.await.expect("node task panicked");
        Self::spawn_in(data_dir)
    }

    pub fn send(&self, command: Command) {
        self.commands.send(command).expect("node running");
    }

    pub async fn expect<T>(&mut self, mut pick: impl FnMut(Event) -> Option<T>) -> T {
        tokio::time::timeout(EVENT_TIMEOUT, async {
            loop {
                let event = self.events.recv().await.expect("node running");
                if let Event::Error(err) = &event {
                    panic!("node error: {err}");
                }
                if let Some(value) = pick(event) {
                    return value;
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

    pub async fn ready(&mut self) {
        self.expect(|event| matches!(event, Event::Ready { .. }).then_some(())).await;
    }

    pub async fn create_identity(&mut self, name: &str) -> UserKey {
        self.ready().await;
        self.send(Command::CreateIdentity { display_name: name.into() });
        self.expect(|event| match event {
            Event::IdentityCreated { profile, .. } => Some(profile.user_key),
            _ => None,
        })
        .await
    }

    pub async fn create_invite(&mut self) -> String {
        self.send(Command::CreateInvite);
        self.expect(|event| match event {
            Event::InviteCreated { link } => Some(link),
            _ => None,
        })
        .await
    }

    pub async fn contact_added(&mut self) -> ContactView {
        self.expect(|event| match event {
            Event::ContactAdded(contact) => Some(contact),
            _ => None,
        })
        .await
    }

    pub async fn received_text(&mut self) -> String {
        self.expect(|event| match event {
            Event::MessageAdded(message) if message.direction == brumm_core::api::Direction::Incoming => Some(message.text),
            _ => None,
        })
        .await
    }

    pub async fn delivered(&mut self) {
        self.expect(|event| match event {
            Event::MessageStatusChanged { status: brumm_core::api::DeliveryStatus::Delivered, .. } => Some(()),
            _ => None,
        })
        .await
    }
}
