//! Main layout: contact sidebar on the left, the selected chat in the center, dialogs on top.

use brumm_core::api::Command;

use crate::{
    state::AppState,
    ui::{ViewState, add_contact, chat, seed_phrase, sidebar},
};

pub fn show(ui: &mut egui::Ui, state: &mut AppState, view: &mut ViewState, commands: &mut Vec<Command>) {
    sidebar::show(ui, state, &mut view.add_contact, commands);
    egui::CentralPanel::default().show(ui, |ui| match state.selected {
        Some(contact) => chat::show(ui, state, contact, view.drafts.entry(contact).or_default(), commands),
        None => empty_state(ui),
    });
    add_contact::show(ui, state, &mut view.add_contact, commands);
    seed_phrase::show(ui, state);
}

fn empty_state(ui: &mut egui::Ui) {
    ui.centered_and_justified(|ui| {
        ui.label("Pick a chat — or add your first contact with “＋ Add contact”.");
    });
}
