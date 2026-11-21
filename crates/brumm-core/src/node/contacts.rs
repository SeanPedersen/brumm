//! Invite handshake (SPEC.md §5.1), both directions: accepting someone's invite (outbound
//! Hello) and answering a Hello for one of our invites. New contacts are synced to our other
//! devices.

use anyhow::{Context, Result, bail};
use iroh::EndpointId;

use super::{Node, Outcome};
use crate::{
    api::{Event, Trust},
    clock::now_ms,
    crypto::{self, Sealed},
    identity::{Profile, UserKey},
    invite::Invite,
    wire::{self, Avatar, ContactRecord, Frame, InviteSecret, Payload, RejectReason, Reply},
};

impl Node {
    pub(super) fn accept_invite(&mut self, link: &str) -> Result<()> {
        let invite = Invite::from_link(link)?;
        let me = self.active_me()?;
        if invite.user_key() == me.user_key() {
            bail!("that is your own invite");
        }
        let mut session = me
            .crypto
            .start_outbound(&invite.device_cert.olm_identity_key, &invite.one_time_key)?;
        let frame = Frame::Hello {
            invite_secret: invite.secret,
            sealed: crypto::seal(&mut session, &wire::encode(&me.profile))?,
        };
        let transport = me.transport.clone();
        self.pending_hellos.insert(invite.device_addr.id, session);
        let outcomes = self.outcomes_tx.clone();
        n0_future::task::spawn(async move {
            let reply = transport.request(invite.device_addr.clone(), &frame).await;
            let _ = outcomes.send(Outcome::Hello {
                invite: Box::new(invite),
                reply,
            });
        });
        Ok(())
    }

    pub(super) fn finish_hello(&mut self, invite: Invite, reply: Result<Reply>) -> Result<()> {
        let device = invite.device_addr.id;
        let mut session = self
            .pending_hellos
            .remove(&device)
            .context("no pending invite for this device")?;
        let sealed = match reply.context("could not reach the inviter — are they online?")? {
            Reply::HelloAck { sealed } => sealed,
            Reply::Rejected(RejectReason::UnknownInvite) => {
                bail!("invite was already used or has expired")
            }
            other => bail!("inviter refused the connection: {other:?}"),
        };
        let profile: Profile = wire::decode(&crypto::open(&mut session, &sealed)?)?;
        let matches_invite = profile.user_key() == invite.user_key()
            && profile.is_valid_for(device)
            && profile
                .devices
                .entry(&device)
                .is_some_and(|entry| entry.cert == invite.device_cert);
        if !matches_invite {
            bail!("inviter answered with a different identity");
        }
        self.add_contact(&profile)?;
        let Self { store, me, .. } = self;
        me.as_mut()
            .context("no identity")?
            .crypto
            .adopt(store, device, session)?;
        self.send_own_avatar_to(&profile.user_key())?;
        self.flush_outbox()
    }

    pub(super) fn accept_hello(
        &mut self,
        from: EndpointId,
        invite_secret: InviteSecret,
        sealed: &Sealed,
    ) -> Result<Reply> {
        self.active_me()?;
        if sealed.pre_key_sender().is_none() {
            return Ok(Reply::Rejected(RejectReason::InvalidProfile));
        }
        if !self.store.take_invite(&invite_secret, now_ms())? {
            return Ok(Reply::Rejected(RejectReason::UnknownInvite));
        }
        let Self { store, me, .. } = self;
        let me = me.as_mut().expect("checked by active_me");
        let (mut session, plaintext, sender_identity) = me.crypto.accept_inbound(store, sealed)?;
        let profile: Profile = wire::decode(&plaintext)?;
        let certified = profile
            .devices
            .entry(&from)
            .is_some_and(|entry| entry.cert.olm_identity_key == sender_identity);
        if !profile.is_valid_for(from) || !certified || profile.user_key() == me.user_key() {
            return Ok(Reply::Rejected(RejectReason::InvalidProfile));
        }
        let ack = crypto::seal(&mut session, &wire::encode(&me.profile))?;
        me.crypto.adopt(store, from, session)?;
        self.add_contact(&profile)?;
        self.send_own_avatar_to(&profile.user_key())?;
        self.flush_outbox()?;
        Ok(Reply::HelloAck { sealed: ack })
    }

    /// A new contact has no photo of ours yet.
    fn send_own_avatar_to(&mut self, contact: &UserKey) -> Result<()> {
        let me = self.me()?.user_key();
        let Some((_, image)) = self
            .store
            .avatars()?
            .into_iter()
            .find(|(user, _)| *user == me)
        else {
            return Ok(());
        };
        self.enqueue_to_user(
            contact,
            rand::random(),
            Payload::Avatar(Avatar { subject: me, image }),
            None,
        )
    }

    /// Stores a verified contact, tells the UI and syncs it to our other devices.
    pub(super) fn add_contact(&mut self, profile: &Profile) -> Result<()> {
        let added_at_ms = now_ms();
        let (contact, removed) =
            self.store
                .upsert_contact(profile, Trust::Verified, added_at_ms)?;
        self.forget_devices(&removed)?;
        self.events.emit(Event::ContactAdded(contact));
        self.enqueue_to_own_devices(Payload::ContactSync(ContactRecord {
            profile: profile.clone(),
            added_at_ms,
        }))
    }
}
