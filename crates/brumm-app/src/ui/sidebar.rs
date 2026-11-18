//! Left panel: own profile, add-contact entry point and the contact list with unread badges.

use brumm_core::api::Command;

use crate::{
    state::AppState,
    ui::{add_contact::AddContactView, theme},
};

const SIDEBAR_WIDTH: f32 = 260.0;

pub fn show(ui: &mut egui::Ui, state: &mut AppState, add_contact: &mut AddContactView, commands: &mut Vec<Command>) {
    egui::Panel::left("contacts")
        .resizable(true)
        .default_size(SIDEBAR_WIDTH)
        .show(ui, |ui| {
            if let Some(profile) = &state.profile {
                ui.add_space(6.0);
                ui.heading(&profile.display_name);
                ui.label(egui::RichText::new(&profile.fingerprint).monospace().small().color(theme::MUTED_TEXT))
                    .on_hover_text("Your fingerprint — compare it with contacts to verify each other");
            }
            ui.add_space(6.0);
            if ui.add_sized([ui.available_width(), 28.0], egui::Button::new("＋ Add contact")).clicked() {
                add_contact.open = true;
            }
            ui.separator();

            let mut clicked = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for contact in &state.contacts {
                    let unread = state.unread.get(&contact.user_key).copied().unwrap_or(0);
                    let label = match unread {
                        0 => contact.display_name.clone(),
                        count => format!("{}  ({count})", contact.display_name),
                    };
                    let selected = state.selected == Some(contact.user_key);
                    let response = ui.add_sized([ui.available_width(), 32.0], egui::Button::selectable(selected, label));
                    if response.clicked() {
                        clicked = Some(contact.user_key);
                    }
                }
            });
            if let Some(contact) = clicked {
                state.select(contact);
                commands.push(Command::LoadConversation { contact });
            }
        });
}
