//! brumm-core: identity, contact establishment, P2P transport and storage for the brumm
//! messenger, with Olm end-to-end encryption per device pair. UIs talk to it only through [`api::Command`] / [`api::Event`] and [`node::run`].
//! See SPEC.md for the protocol.

pub mod api;
pub mod clock;
pub mod crypto;
mod e2ee;
pub mod identity;
pub mod invite;
pub mod link;
pub mod mailbox;
pub mod node;
mod reconcile;
mod store;
mod transport;
pub mod wire;

#[cfg(target_family = "wasm")]
#[doc(hidden)]
pub use universal_time as __universal_time;
