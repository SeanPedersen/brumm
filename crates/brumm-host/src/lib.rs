//! brumm-host: native-only glue between brumm-core and the programs that host it, i.e. the
//! desktop app and the headless hosts for software, bots and agents (PLAN-AGENTS.md, #14).
//!
//! - [`lock`]: one running core per profile directory.
//! - [`client`]: an async request/response API over the core's `Command`/`Event` channel.
//! - [`mirror`]: the profile, contacts, groups and invitations the core reported.
//! - [`policy`] + [`enforcer`]: auto-accept and lock rules (`policy.toml`) and their application.
//! - [`dto`]: stable JSON views (ids as base32 strings) that do not change with core types.

pub mod client;
pub mod dto;
pub mod enforcer;
pub mod lock;
pub mod mirror;
pub mod policy;

pub use client::{Client, Handle, Outgoing, Page};
pub use lock::ProfileLock;
pub use mirror::Snapshot;
pub use policy::Policy;
