//! Invite tickets: everything a peer needs to dial the inviting device once, prove it was
//! invited and open an Olm session to it (via the one-time key). The full profile with all
//! devices is exchanged inside the encrypted Hello / HelloAck, which keeps invites QR-sized.

use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::{
    crypto::OneTimeKey,
    identity::{DeviceCert, UserKey, normalize_display_name},
    wire::{self, InviteSecret, UriError},
};

const INVITE_PREFIX: &str = "brumm://invite/";
const INVITE_VERSION: u8 = 3;
pub const INVITE_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub version: u8,
    pub display_name: String,
    pub device_cert: DeviceCert,
    pub device_addr: EndpointAddr,
    pub secret: InviteSecret,
    pub one_time_key: OneTimeKey,
}

#[derive(Debug, thiserror::Error)]
pub enum InviteError {
    #[error(transparent)]
    Uri(#[from] UriError),
    #[error("invite was created by a different brumm version")]
    UnsupportedVersion,
    #[error("invite signature is invalid")]
    InvalidCert,
}

impl Invite {
    pub fn new(display_name: String, device_cert: DeviceCert, device_addr: EndpointAddr, secret: InviteSecret, one_time_key: OneTimeKey) -> Self {
        Self { version: INVITE_VERSION, display_name, device_cert, device_addr, secret, one_time_key }
    }

    pub fn user_key(&self) -> UserKey {
        self.device_cert.user_key
    }

    pub fn to_link(&self) -> String {
        wire::to_uri(INVITE_PREFIX, self)
    }

    pub fn from_link(link: &str) -> Result<Self, InviteError> {
        let invite: Invite = wire::from_uri(INVITE_PREFIX, link)?;
        if invite.version != INVITE_VERSION {
            return Err(InviteError::UnsupportedVersion);
        }
        let consistent = invite.device_cert.verify()
            && invite.device_cert.device_id == invite.device_addr.id
            && normalize_display_name(&invite.display_name).as_ref() == Some(&invite.display_name);
        if !consistent {
            return Err(InviteError::InvalidCert);
        }
        Ok(invite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::UserIdentity;

    #[test]
    fn link_roundtrip() {
        let identity = UserIdentity::generate();
        let device = iroh::SecretKey::generate().public();
        let cert = identity.certify_device(device, [3; 32], 1);
        let invite = Invite::new("alice".into(), cert, EndpointAddr::from(device), [9; 16], [8; 32]);
        assert_eq!(Invite::from_link(&invite.to_link()).unwrap(), invite);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(Invite::from_link("hello"), Err(InviteError::Uri(UriError::WrongPrefix))));
        assert!(matches!(Invite::from_link("brumm://invite/!!!"), Err(InviteError::Uri(UriError::Malformed))));
    }
}
