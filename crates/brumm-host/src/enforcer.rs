//! Applies a profile's [`Policy`] while a host runs it: open group invitations are accepted or
//! declined as [`policy::decide`] says (each one once), when they arrive and again whenever the
//! policy changes. Runs until the core stops.

use std::collections::HashSet;

use brumm_core::api::{Command, Event, GroupId, GroupInvitationView, InvitationStage};
use tokio::{
    sync::{broadcast::error::RecvError, watch},
    task::JoinHandle,
};

use crate::{
    client::Handle,
    policy::{self, Decision, Policy},
};

/// The policy is shared: a change (`policy.set`) is applied to open invitations right away.
pub type SharedPolicy = watch::Sender<Policy>;

pub fn spawn(handle: Handle, policy: &SharedPolicy) -> JoinHandle<()> {
    let mut events = handle.events();
    let mut policy = policy.subscribe();
    tokio::spawn(async move {
        let mut answered = HashSet::new();
        answer(&handle, &policy.borrow_and_update(), &handle.snapshot().invitations, &mut answered);
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(Event::GroupInvitations(invitations)) => {
                        answer(&handle, &policy.borrow(), &invitations, &mut answered);
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => {
                        answer(&handle, &policy.borrow(), &handle.snapshot().invitations, &mut answered);
                    }
                    Err(RecvError::Closed) => return,
                },
                changed = policy.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    answer(&handle, &policy.borrow_and_update(), &handle.snapshot().invitations, &mut answered);
                }
            }
        }
    })
}

fn answer(
    handle: &Handle,
    policy: &Policy,
    invitations: &[GroupInvitationView],
    answered: &mut HashSet<GroupId>,
) {
    let contacts = handle.snapshot().contacts;
    for invitation in invitations {
        if invitation.stage != InvitationStage::Open || answered.contains(&invitation.group) {
            continue;
        }
        let group = invitation.group;
        let command = match policy::decide(policy, &invitation.inviter, &contacts) {
            Decision::Accept => Command::AcceptGroupInvitation { group },
            Decision::Decline => Command::DeclineGroupInvitation { group },
            Decision::Ask => continue,
        };
        answered.insert(group);
        // A stopped core ends the loop on the next receive.
        let _ = handle.command(command);
    }
}
