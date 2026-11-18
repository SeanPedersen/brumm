//! Invite tickets: everything a peer needs to dial us once and prove it was invited.
//! Encoded as `brumm://invite/<base32(postcard)>` for links and QR codes.

use data_encoding::BASE32_NOPAD;
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::{identity::Profile, wire::InviteSecret};

const INVITE_PREFIX: &str = "brumm://invite/";
const INVITE_VERSION: u8 = 1;
pub const INVITE_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub version: u8,
    pub profile: Profile,
    pub device_addr: EndpointAddr,
    pub secret: InviteSecret,
}

#[derive(Debug, thiserror::Error)]
pub enum InviteError {
    #[error("not a brumm invite link")]
    MissingPrefix,
    #[error("invite link is corrupted")]
    Malformed,
    #[error("invite was created by a newer brumm version")]
    UnsupportedVersion,
    #[error("invite signature is invalid")]
    InvalidProfile,
}

impl Invite {
    pub fn new(profile: Profile, device_addr: EndpointAddr, secret: InviteSecret) -> Self {
        Self {
            version: INVITE_VERSION,
            profile,
            device_addr,
            secret,
        }
    }

    pub fn to_link(&self) -> String {
        let payload = BASE32_NOPAD.encode(&crate::wire::encode(self));
        format!("{INVITE_PREFIX}{}", payload.to_ascii_lowercase())
    }

    pub fn from_link(link: &str) -> Result<Self, InviteError> {
        let payload = link
            .trim()
            .strip_prefix(INVITE_PREFIX)
            .ok_or(InviteError::MissingPrefix)?;
        let bytes = BASE32_NOPAD
            .decode(payload.to_ascii_uppercase().as_bytes())
            .map_err(|_| InviteError::Malformed)?;
        let invite: Invite = crate::wire::decode(&bytes).map_err(|_| InviteError::Malformed)?;
        if invite.version != INVITE_VERSION {
            return Err(InviteError::UnsupportedVersion);
        }
        if !invite.profile.is_valid_for(invite.device_addr.id) {
            return Err(InviteError::InvalidProfile);
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
        let profile = Profile {
            display_name: "alice".into(),
            device_cert: identity.certify_device(device, 1),
        };
        let invite = Invite::new(profile, EndpointAddr::from(device), [9; 16]);
        let decoded = Invite::from_link(&invite.to_link()).unwrap();
        assert_eq!(decoded, invite);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(Invite::from_link("hello"), Err(InviteError::MissingPrefix)));
        assert!(matches!(
            Invite::from_link("brumm://invite/!!!"),
            Err(InviteError::Malformed)
        ));
    }
}
