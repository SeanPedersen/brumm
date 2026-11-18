//! Device linking (SPEC.md §3.4): a fresh device shows a [`LinkTicket`]; an already linked
//! device pastes/scans it, signs a new device list including the new device and sends a
//! sealed [`LinkBundle`] carrying the user identity. Contacts and history follow as sealed
//! own-device sync envelopes.

use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::{
    crypto::{OlmIdentityKey, OneTimeKey},
    identity::{DeviceList, MAX_DEVICE_LABEL_CHARS},
    mailbox::MailboxAddress,
    wire::{self, LinkSecret, UriError},
};

const LINK_PREFIX: &str = "brumm://link/";
const LINK_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkTicket {
    pub version: u8,
    pub device_addr: EndpointAddr,
    pub link_secret: LinkSecret,
    pub olm_identity_key: OlmIdentityKey,
    pub one_time_key: OneTimeKey,
    pub fallback_key: OneTimeKey,
    pub mailbox: MailboxAddress,
    pub label: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error(transparent)]
    Uri(#[from] UriError),
    #[error("link was created by a different brumm version")]
    UnsupportedVersion,
    #[error("device label is invalid")]
    InvalidLabel,
    #[error("mailbox address is invalid")]
    InvalidMailbox,
}

impl LinkTicket {
    pub fn to_link(&self) -> String {
        wire::to_uri(LINK_PREFIX, self)
    }

    pub fn from_link(link: &str) -> Result<Self, LinkError> {
        let ticket: LinkTicket = wire::from_uri(LINK_PREFIX, link)?;
        if ticket.version != LINK_VERSION {
            return Err(LinkError::UnsupportedVersion);
        }
        let label_ok = !ticket.label.trim().is_empty() && ticket.label.chars().count() <= MAX_DEVICE_LABEL_CHARS;
        if !label_ok {
            return Err(LinkError::InvalidLabel);
        }
        if !ticket.mailbox.is_valid() {
            return Err(LinkError::InvalidMailbox);
        }
        Ok(ticket)
    }

    pub fn new(device_addr: EndpointAddr, link_secret: LinkSecret, olm_identity_key: OlmIdentityKey, one_time_key: OneTimeKey, fallback_key: OneTimeKey, mailbox: MailboxAddress, label: String) -> Self {
        Self { version: LINK_VERSION, device_addr, link_secret, olm_identity_key, one_time_key, fallback_key, mailbox, label }
    }
}

/// Sealed to the new device: the user identity plus the device list that includes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkBundle {
    pub seed_phrase: String,
    pub display_name: String,
    pub devices: DeviceList,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_roundtrip() {
        let device = iroh::SecretKey::generate().public();
        let mailbox = crate::mailbox::address(&[5; 32], crate::mailbox::default_relays());
        let ticket = LinkTicket::new(EndpointAddr::from(device), [1; 16], [2; 32], [3; 32], [4; 32], mailbox, "Linux".into());
        assert_eq!(LinkTicket::from_link(&ticket.to_link()).unwrap(), ticket);
        assert!(LinkTicket::from_link("brumm://invite/abc").is_err());
    }
}
