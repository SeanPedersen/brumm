//! Orderly stop. Offline delivery normally starts only after a direct attempt failed, which
//! takes seconds; a user who quits right after sending would leave the message in the outbox
//! until the next launch. On shutdown every envelope not yet on the relays is therefore posted
//! to its device's mailbox at once (no direct attempt), and the results are awaited, capped.

use std::{collections::HashSet, pin::pin, time::Duration};

use anyhow::Result;
use n0_future::time;

use super::Node;
use crate::wire::EnvelopeId;
use iroh::EndpointId;

/// Upper bound for the drain: a relay that never answers must not keep the process alive.
pub const SHUTDOWN_DRAIN_MAX: Duration = Duration::from_secs(10);

impl Node {
    /// Posts the whole outbox to the mailboxes and waits until every post has finished.
    pub(super) async fn drain_outbox(&mut self) {
        if self.me.as_ref().is_none_or(|me| me.revoked) {
            return;
        }
        let mut deadline = pin!(time::sleep(SHUTDOWN_DRAIN_MAX));
        let mut attempted = HashSet::new();
        loop {
            match self.post_unattempted(&mut attempted) {
                Ok(false) => return,
                Ok(true) => {}
                Err(err) => {
                    tracing::warn!(?err, "could not drain the outbox on shutdown");
                    return;
                }
            }
            tokio::select! {
                Some(outcome) = self.outcomes_rx.recv() => {
                    let result = self.handle_outcome(outcome);
                    self.report(result);
                }
                _ = &mut deadline => {
                    tracing::info!("shutdown drain timed out, the rest stays in the outbox");
                    return;
                }
            }
        }
    }

    /// Starts a mailbox post for every queued envelope not tried yet during this drain.
    /// Returns whether something is still to wait for (a post running, or the mailbox not
    /// connected yet).
    fn post_unattempted(
        &mut self,
        attempted: &mut HashSet<(EndpointId, EnvelopeId)>,
    ) -> Result<bool> {
        let mailbox_ready = self.me.as_ref().is_some_and(|me| me.mailbox.is_some());
        let mut waiting_for_mailbox = false;
        for key in self.store.outbox_keys()? {
            let (device, envelope_id) = key;
            if attempted.contains(&key) {
                continue;
            }
            let pending = self
                .store
                .outbox_item(&device, &envelope_id)?
                .is_some_and(|item| !item.mailboxed);
            if !pending {
                continue;
            }
            if !mailbox_ready {
                waiting_for_mailbox = true;
                continue;
            }
            attempted.insert(key);
            self.post_to_mailbox(device, envelope_id)?;
        }
        Ok(waiting_for_mailbox || !self.mailboxing.is_empty())
    }
}
