//! Logging out of this device: it signs a device list without itself (any device can, it holds
//! the seed), tells its other devices and every contact, then erases all local data.
//!
//! Erasing waits until no delivery or mailbox post is in flight anymore, or until
//! [`LOGOUT_GRACE`] passed, so the announcement gets a fair chance to leave. Devices that miss
//! it learn the new list from our other devices later (or keep encrypting to a dead device,
//! which only costs them unread mailbox letters).

use std::time::Duration;

use anyhow::Result;

use super::{Node, Outcome, linking::other_devices, next_seq, shutdown_network};
use crate::api::Event;

/// Longest wait for the removal announcement before the data is erased anyway.
const LOGOUT_GRACE: Duration = Duration::from_secs(20);

impl Node {
    pub(super) fn begin_logout(&mut self) -> Result<()> {
        self.logging_out = true;
        let outcomes = self.outcomes_tx.clone();
        n0_future::task::spawn(async move {
            n0_future::time::sleep(LOGOUT_GRACE).await;
            let _ = outcomes.send(Outcome::LogoutDeadline);
        });

        let Some(me) = self.me.as_mut().filter(|me| !me.revoked) else {
            return Ok(());
        };
        let remaining = other_devices(&me.profile.devices, &me.device_id);
        if remaining.is_empty() {
            // Our only device: nobody to tell; restoring from the seed later signs a newer list.
            return Ok(());
        }
        let seq = next_seq(me.profile.devices.seq);
        // In memory only: the store is erased next.
        me.profile.devices = me.user.sign_device_list(seq, remaining);
        tracing::info!("logging out: announcing removal of this device");
        self.announce_device_list()?;
        self.flush_outbox()
    }

    /// Logging out, and no delivery is running anymore (or the grace period ended).
    pub(super) fn logout_ready(&self) -> bool {
        self.logging_out
            && (self.logout_deadline_passed
                || (self.in_flight.is_empty() && self.mailboxing.is_empty()))
    }

    /// Stops the network, erases the database and returns a fresh node (no identity).
    pub(super) async fn finish_logout(self) -> Result<Node> {
        let Node {
            store,
            events,
            mailbox_relays,
            me,
            link_target,
            ..
        } = self;
        shutdown_network(me, link_target).await;
        store.wipe()?;
        tracing::info!("logged out; local data erased");
        let fresh = Node::with_store(store, events.clone(), mailbox_relays);
        events.emit(Event::LoggedOut);
        Ok(fresh)
    }
}
