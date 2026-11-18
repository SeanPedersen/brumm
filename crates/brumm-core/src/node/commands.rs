//! UI command handling: identity creation/restore, invites and sending text.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use iroh::SecretKey;

use super::{Me, Node, next_seq, profile_view};
use crate::{
    api::{Command, DeliveryStatus, Direction, Event, MessageView},
    clock::now_ms,
    e2ee::DeviceCrypto,
    identity::{DeviceEntry, Profile, UserIdentity, UserKey, default_device_label, normalize_display_name},
    invite::Invite,
    mailbox,
    store::StoredIdentity,
    transport::Transport,
    wire::{Body, ChatMessage, Payload},
};

impl Node {
    pub(super) async fn handle_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::CreateIdentity { display_name } => {
                self.create_identity(UserIdentity::generate(), &display_name, true).await
            }
            Command::RestoreIdentity { phrase, display_name } => {
                let user = UserIdentity::from_phrase(&phrase).context("invalid seed phrase")?;
                self.create_identity(user, &display_name, false).await
            }
            Command::StartLinking => self.start_linking().await,
            Command::CreateInvite => self.create_invite(),
            Command::AcceptInvite { link } => self.accept_invite(&link),
            Command::LinkDevice { link } => self.link_device(&link),
            Command::RemoveDevice { device_id } => self.remove_device(device_id),
            Command::LoadConversation { contact } => {
                let messages = self.store.conversation(&contact)?;
                self.events.emit(Event::Conversation { contact, messages });
                Ok(())
            }
            Command::SendText { contact, text } => self.send_text(contact, text),
        }
    }

    async fn create_identity(&mut self, user: UserIdentity, display_name: &str, is_new: bool) -> Result<()> {
        if self.me.is_some() || self.link_target.is_some() {
            bail!("this device already has (or is joining) an identity");
        }
        let display_name = normalize_display_name(display_name).context("display name is empty or invalid")?;
        let device_secret = SecretKey::generate();
        let (crypto, fallback_key) = DeviceCrypto::generate(&device_secret.to_bytes());
        let entry = DeviceEntry {
            cert: user.certify_device(device_secret.public(), crypto.identity_key(), now_ms()),
            fallback_key,
            mailbox: mailbox::address(&device_secret.to_bytes(), self.mailbox_relays.clone()),
            label: default_device_label(),
        };
        let profile = Profile {
            display_name,
            devices: user.sign_device_list(next_seq(0), vec![entry]),
        };
        self.store.save_identity(&StoredIdentity {
            seed_phrase: user.phrase(),
            device_secret: device_secret.clone(),
            profile: profile.clone(),
            olm_account: crypto.account_pickle(),
        })?;
        let transport = Arc::new(Transport::bind(device_secret.clone(), self.inbound_tx.clone()).await?);
        self.start_mailbox(&device_secret, self.mailbox_relays.clone());
        let seed_phrase = is_new.then(|| user.phrase());
        self.events.emit(Event::IdentityCreated { profile: profile_view(&profile), seed_phrase });
        self.me = Some(Me {
            user,
            profile,
            device_id: device_secret.public(),
            crypto,
            transport,
            mailbox: None,
            revoked: false,
        });
        self.emit_devices();
        Ok(())
    }

    fn create_invite(&mut self) -> Result<()> {
        self.active_me()?;
        let secret: [u8; 16] = rand::random();
        let Self { store, me, events, .. } = self;
        let me = me.as_mut().expect("checked by active_me");
        let one_time_key = me.crypto.new_one_time_key(store)?;
        store.add_invite(&secret, now_ms())?;
        let transport = me.transport.clone();
        let display_name = me.profile.display_name.clone();
        let cert = me.own_cert().clone();
        let events = events.clone();
        tokio::spawn(async move {
            let invite = Invite::new(display_name, cert, transport.addr().await, secret, one_time_key);
            events.emit(Event::InviteCreated { link: invite.to_link() });
        });
        Ok(())
    }

    fn send_text(&mut self, contact: UserKey, text: String) -> Result<()> {
        self.active_me()?;
        self.store.contact(&contact)?.context("unknown contact")?;
        let message = ChatMessage {
            id: rand::random(),
            sent_at_ms: now_ms(),
            body: Body::Text(text.trim().to_owned()),
        };
        if !message.body.is_valid() {
            bail!("message is empty or too long");
        }
        self.store.insert_message(&contact, &message, Direction::Outgoing, DeliveryStatus::Pending, None)?;
        self.events.emit(Event::MessageAdded(message_view(contact, &message, Direction::Outgoing, DeliveryStatus::Pending)));
        self.enqueue_to_user(&contact, message.id, Payload::Chat(message.clone()), Some(&contact))?;
        self.enqueue_to_own_devices(Payload::SentTranscript { contact, message })?;
        self.flush_outbox()
    }
}

pub(super) fn message_view(contact: UserKey, message: &ChatMessage, direction: Direction, status: DeliveryStatus) -> MessageView {
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
