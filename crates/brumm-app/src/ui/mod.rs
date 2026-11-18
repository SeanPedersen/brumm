//! Screens and widgets. Each screen is a function of (state, view-local state) that pushes
//! [`brumm_core::api::Command`]s; none of them own core state.

pub mod add_contact;
pub mod chat;
pub mod devices;
pub mod errors;
pub mod main_screen;
pub mod onboarding;
pub mod qr;
pub mod revoked;
pub mod seed_phrase;
pub mod sidebar;
pub mod starting;
pub mod theme;

use std::collections::HashMap;

use brumm_core::identity::UserKey;

/// Transient, view-only state (text drafts, open dialogs, cached textures).
#[derive(Default)]
pub struct ViewState {
    pub onboarding: onboarding::OnboardingView,
    pub add_contact: add_contact::AddContactView,
    pub devices: devices::DevicesView,
    pub drafts: HashMap<UserKey, String>,
}
