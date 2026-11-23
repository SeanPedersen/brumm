//! Host-side rules for one profile, kept in `policy.toml` in its directory (no core change):
//! whose group invitations are accepted without a click, and lock mode for bots.
//!
//! ```toml
//! locked = true              # refuse new invites and contacts; group invitations only from `allow`
//! allow = ["<user key>"]     # base32 user keys; empty means the first contact (who paired the bot)
//! auto_accept = true         # accept invitations from allowed users right away
//! ```
//!
//! The rules ([`decide`], [`check`]) are pure; [`load`] / [`save`] do the file I/O.

use std::{fs, io::ErrorKind, path::Path};

use anyhow::{Context, Result};
use brumm_core::{api::ContactView, identity::UserKey};
use serde::{Deserialize, Serialize};

use crate::dto::encode_id;

pub const POLICY_FILE: &str = "policy.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub locked: bool,
    pub allow: Vec<String>,
    pub auto_accept: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            locked: false,
            allow: Vec::new(),
            auto_accept: true,
        }
    }
}

/// What to do with an open group invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    Decline,
    /// Leave it open for a person or program to answer.
    Ask,
}

/// Things lock mode refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    CreateInvite,
    AcceptInvite,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("this profile is locked: {0} is refused (unlock with `brumm-cli policy --unlock`)")]
pub struct Refused(&'static str);

/// Whether `inviter` is on the allowlist (empty list: the first contact).
pub fn is_allowed(policy: &Policy, inviter: &UserKey, contacts: &[ContactView]) -> bool {
    if policy.allow.is_empty() {
        return contacts
            .iter()
            .min_by_key(|contact| contact.added_at_ms)
            .is_some_and(|first| first.user_key == *inviter);
    }
    let inviter = encode_id(inviter);
    policy.allow.iter().any(|allowed| allowed.trim().eq_ignore_ascii_case(&inviter))
}

pub fn decide(policy: &Policy, inviter: &UserKey, contacts: &[ContactView]) -> Decision {
    let allowed = is_allowed(policy, inviter, contacts);
    if allowed && policy.auto_accept {
        return Decision::Accept;
    }
    if !allowed && policy.locked {
        return Decision::Decline;
    }
    Decision::Ask
}

pub fn check(policy: &Policy, action: Action) -> Result<(), Refused> {
    if !policy.locked {
        return Ok(());
    }
    Err(Refused(match action {
        Action::CreateInvite => "creating an invite",
        Action::AcceptInvite => "accepting an invite",
    }))
}

pub fn parse(text: &str) -> Result<Policy> {
    Ok(toml::from_str(text)?)
}

pub fn to_text(policy: &Policy) -> Result<String> {
    Ok(toml::to_string(policy)?)
}

/// The profile's policy; the default if it has no `policy.toml`.
pub fn load(profile_dir: &Path) -> Result<Policy> {
    let path = profile_dir.join(POLICY_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => parse(&text).with_context(|| format!("invalid {}", path.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Policy::default()),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

/// Written to a temporary file first, so a crash never leaves half a policy behind.
pub fn save(profile_dir: &Path, policy: &Policy) -> Result<()> {
    fs::create_dir_all(profile_dir).context("create profile dir")?;
    let path = profile_dir.join(POLICY_FILE);
    let partial = path.with_extension("toml.partial");
    fs::write(&partial, to_text(policy)?).context("write policy")?;
    fs::rename(&partial, &path).context("replace policy")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use brumm_core::api::Trust;

    use super::*;

    fn contact(byte: u8, added_at_ms: u64) -> ContactView {
        ContactView {
            user_key: [byte; 32],
            display_name: String::new(),
            fingerprint: String::new(),
            trust: Trust::Verified,
            added_at_ms,
        }
    }

    #[test]
    fn the_first_contact_is_allowed_by_default() {
        let contacts = [contact(2, 20), contact(1, 10)];
        let open = Policy::default();
        assert_eq!(decide(&open, &[1; 32], &contacts), Decision::Accept);
        assert_eq!(decide(&open, &[2; 32], &contacts), Decision::Ask);
        let locked = Policy {
            locked: true,
            ..Policy::default()
        };
        assert_eq!(decide(&locked, &[1; 32], &contacts), Decision::Accept);
        assert_eq!(decide(&locked, &[2; 32], &contacts), Decision::Decline);
        let asking = Policy {
            auto_accept: false,
            ..locked
        };
        assert_eq!(decide(&asking, &[1; 32], &contacts), Decision::Ask);
    }

    #[test]
    fn an_explicit_allowlist_replaces_the_first_contact() {
        let contacts = [contact(1, 10), contact(2, 20)];
        let policy = Policy {
            locked: true,
            allow: vec![encode_id(&[2; 32]).to_uppercase()],
            auto_accept: true,
        };
        assert_eq!(decide(&policy, &[2; 32], &contacts), Decision::Accept);
        assert_eq!(decide(&policy, &[1; 32], &contacts), Decision::Decline);
    }

    #[test]
    fn lock_mode_refuses_invites() {
        assert_eq!(check(&Policy::default(), Action::CreateInvite), Ok(()));
        let locked = Policy {
            locked: true,
            ..Policy::default()
        };
        assert!(check(&locked, Action::CreateInvite).is_err());
        assert!(check(&locked, Action::AcceptInvite).is_err());
    }

    #[test]
    fn text_round_trips_and_rejects_typos() {
        assert_eq!(parse("").unwrap(), Policy::default());
        let policy = Policy {
            locked: true,
            allow: vec!["abc".into()],
            auto_accept: false,
        };
        assert_eq!(parse(&to_text(&policy).unwrap()).unwrap(), policy);
        assert!(parse("lockd = true").is_err());
    }
}
