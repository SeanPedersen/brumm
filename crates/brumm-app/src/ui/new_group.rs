//! "New group" window: a name, announcement mode, and the contacts to invite. Each invited
//! contact joins once it accepts the invitation.

use std::collections::HashSet;

use brumm_core::{api::Command, identity::UserKey};

use crate::{state::AppState, ui::theme};

const WINDOW_WIDTH: f32 = 360.0;
const CONTACT_LIST_HEIGHT: f32 = 240.0;
/// Same bound as the core's group name check.
const MAX_NAME_CHARS: usize = 64;

#[derive(Default)]
pub struct NewGroupView {
    open: bool,
    name: String,
    announcement: bool,
    filter: String,
    picked: HashSet<UserKey>,
}

impl NewGroupView {
    pub fn open(&mut self) {
        *self = Self {
            open: true,
            ..Self::default()
        };
    }
}

pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    view: &mut NewGroupView,
    commands: &mut Vec<Command>,
) {
    if !view.open {
        return;
    }
    let mut open = view.open;
    let mut created = false;
    theme::dialog("New group", ui.ctx(), WINDOW_WIDTH)
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut view.name)
                    .hint_text("Group name")
                    .char_limit(MAX_NAME_CHARS)
                    .desired_width(f32::INFINITY),
            );
            ui.checkbox(
                &mut view.announcement,
                "Only admins can post (announcements)",
            );
            ui.separator();
            ui.label(egui::RichText::new("Invite contacts").strong());
            ui.add(
                egui::TextEdit::singleline(&mut view.filter)
                    .hint_text("Search contacts")
                    .desired_width(f32::INFINITY),
            );
            contact_picker(ui, state, &view.filter, &mut view.picked);
            ui.add_space(8.0);
            let name = view.name.trim();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("{} selected", view.picked.len()))
                        .small()
                        .color(theme::MUTED_TEXT),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(!name.is_empty(), egui::Button::new("Create"))
                        .clicked()
                    {
                        commands.push(Command::CreateGroup {
                            name: name.to_owned(),
                            announcement: view.announcement,
                            members: view.picked.iter().copied().collect(),
                        });
                        created = true;
                    }
                });
            });
        });
    view.open = open && !created;
}

/// Checkbox list of contacts (Notes excluded), filtered by name.
pub fn contact_picker(
    ui: &mut egui::Ui,
    state: &AppState,
    filter: &str,
    picked: &mut HashSet<UserKey>,
) {
    let needle = filter.trim().to_lowercase();
    egui::ScrollArea::vertical()
        .max_height(CONTACT_LIST_HEIGHT)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            let contacts = state.contacts.iter().filter(|contact| {
                !state.is_notes(&brumm_core::api::ConversationId::Direct(contact.user_key))
                    && (needle.is_empty() || contact.display_name.to_lowercase().contains(&needle))
            });
            let mut any = false;
            for contact in contacts {
                any = true;
                let mut checked = picked.contains(&contact.user_key);
                if ui.checkbox(&mut checked, &contact.display_name).changed() {
                    if checked {
                        picked.insert(contact.user_key);
                    } else {
                        picked.remove(&contact.user_key);
                    }
                }
            }
            if !any {
                ui.label(egui::RichText::new("No contacts").color(theme::MUTED_TEXT));
            }
        });
}
