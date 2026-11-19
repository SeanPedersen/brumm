//! Screens and widgets. Each screen is a function of (state, view-local state) that pushes
//! [`brumm_core::api::Command`]s; none of them own core state.

pub mod add_contact;
pub mod avatar;
pub mod bubble;
pub mod chat;
pub mod composer;
pub mod devices;
pub mod emoji;
pub mod errors;
pub mod files;
mod links;
pub mod main_screen;
pub mod onboarding;
pub mod qr;
pub mod revoked;
pub mod seed_phrase;
pub mod sidebar;
pub mod starting;
pub mod theme;
pub mod time_fmt;

use std::collections::HashMap;

use brumm_core::identity::UserKey;

/// Transient, view-only state (text drafts, open dialogs, cached textures).
#[derive(Default)]
pub struct ViewState {
    pub onboarding: onboarding::OnboardingView,
    pub add_contact: add_contact::AddContactView,
    pub devices: devices::DevicesView,
    pub chat: chat::ChatView,
    pub drafts: HashMap<UserKey, String>,
    pub search: String,
    pub remove_avatar_requested: bool,
    /// Save-as targets waiting for the attachment bytes to arrive from the core.
    pub pending_saves: Vec<main_screen::PendingSave>,
}
