//! Screens and widgets. Each screen is a function of (state, view-local state) that pushes
//! [`brumm_core::api::Command`]s; none of them own core state.

pub mod add_contact;
pub mod avatar;
pub mod bubble;
pub mod chat;
mod chat_search;
pub mod composer;
pub mod devices;
pub mod emoji;
pub mod emoji_font;
pub mod errors;
pub mod files;
pub mod group_info;
mod group_rows;
mod icons;
mod links;
pub mod logout;
pub mod main_screen;
pub mod markdown;
pub mod narrow;
pub mod new_group;
pub mod onboarding;
pub mod qr;
pub mod revoked;
pub mod seed_phrase;
pub mod sidebar;
pub mod starting;
mod sync_hint;
pub mod theme;
pub mod time_fmt;

use std::collections::HashMap;

use brumm_core::api::ConversationId;

/// Transient, view-only state (text drafts, open dialogs, cached textures).
#[derive(Default)]
pub struct ViewState {
    pub onboarding: onboarding::OnboardingView,
    pub add_contact: add_contact::AddContactView,
    pub devices: devices::DevicesView,
    pub new_group: new_group::NewGroupView,
    pub group_info: group_info::GroupInfoView,
    pub logout: logout::LogoutView,
    pub chat: chat::ChatView,
    pub narrow: narrow::NarrowNav,
    pub drafts: HashMap<ConversationId, String>,
    pub search: String,
    pub remove_avatar_requested: bool,
}
