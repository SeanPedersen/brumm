//! brumm-core: identity, contact establishment, P2P transport and storage for the brumm
//! messenger. UIs talk to it only through [`api::Command`] / [`api::Event`] and [`node::run`].
//! See SPEC.md for the protocol.

pub mod api;
mod clock;
pub mod identity;
pub mod invite;
pub mod node;
mod store;
mod transport;
pub mod wire;
