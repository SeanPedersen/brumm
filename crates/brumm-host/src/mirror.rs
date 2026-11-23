//! Live copy of what the core reported about the profile: own identity, contacts, groups and
//! open invitations. Pure: it only folds events, so hosts can answer `rooms.list` without
//! asking the core.

use brumm_core::api::{ContactView, Event, GroupInvitationView, GroupView, ProfileView};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub profile: Option<ProfileView>,
    pub contacts: Vec<ContactView>,
    pub groups: Vec<GroupView>,
    pub invitations: Vec<GroupInvitationView>,
}

impl Snapshot {
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Ready {
                profile,
                contacts,
                groups,
                invitations,
                ..
            } => {
                *self = Self {
                    profile: profile.clone(),
                    contacts: contacts.clone(),
                    groups: groups.clone(),
                    invitations: invitations.clone(),
                };
            }
            Event::IdentityCreated { profile, .. } => self.profile = Some(profile.clone()),
            Event::LoggedOut => *self = Self::default(),
            Event::ContactAdded(contact) => {
                match self.contacts.iter_mut().find(|known| known.user_key == contact.user_key) {
                    Some(known) => *known = contact.clone(),
                    None => self.contacts.push(contact.clone()),
                }
            }
            Event::GroupChanged(group) => {
                match self.groups.iter_mut().find(|known| known.id == group.id) {
                    Some(known) => *known = group.clone(),
                    None => self.groups.push(group.clone()),
                }
            }
            Event::GroupInvitations(invitations) => self.invitations = invitations.clone(),
            _ => {}
        }
    }

    /// The contact added first: whoever paired this profile (a bot's owner).
    pub fn first_contact(&self) -> Option<&ContactView> {
        self.contacts.iter().min_by_key(|contact| contact.added_at_ms)
    }
}

#[cfg(test)]
mod tests {
    use brumm_core::api::{GroupStatus, Trust};

    use super::*;

    fn contact(byte: u8, added_at_ms: u64) -> ContactView {
        ContactView {
            user_key: [byte; 32],
            display_name: format!("c{byte}"),
            fingerprint: String::new(),
            trust: Trust::Verified,
            added_at_ms,
        }
    }

    #[test]
    fn folds_contacts_groups_and_logout() {
        let mut snapshot = Snapshot::default();
        snapshot.apply(&Event::ContactAdded(contact(2, 20)));
        snapshot.apply(&Event::ContactAdded(contact(1, 10)));
        snapshot.apply(&Event::ContactAdded(contact(2, 20)));
        assert_eq!(snapshot.contacts.len(), 2);
        assert_eq!(snapshot.first_contact().unwrap().user_key, [1; 32]);
        let mut group = GroupView {
            id: [5; 32],
            name: "a".into(),
            status: GroupStatus::Active,
            my_role: None,
            announcement: false,
            history_sharing: false,
            member_count: 1,
            owner: None,
        };
        snapshot.apply(&Event::GroupChanged(group.clone()));
        group.name = "b".into();
        snapshot.apply(&Event::GroupChanged(group));
        assert_eq!(snapshot.groups.len(), 1);
        assert_eq!(snapshot.groups[0].name, "b");
        snapshot.apply(&Event::LoggedOut);
        assert_eq!(snapshot, Snapshot::default());
    }
}
