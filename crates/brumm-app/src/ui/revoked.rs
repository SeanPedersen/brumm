//! Shown when another device of ours removed this one from the identity. Offers to erase it,
//! which brings back onboarding so it can be linked again.

use brumm_core::api::Command;

use crate::{
    state::AppState,
    ui::{logout, theme},
};

pub fn show(ui: &mut egui::Ui, state: &mut AppState, commands: &mut Vec<Command>) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.heading(egui::RichText::new("This device was removed").color(theme::DANGER));
            ui.label("It no longer sends or receives messages for your account.");
            ui.add_space(16.0);
            let erase = ui.add_enabled(
                !state.logging_out,
                theme::danger_button("Erase this device and start over"),
            );
            if erase.clicked() {
                logout::request(state, commands);
            }
            ui.label(
                egui::RichText::new("Deletes its chats and keys here; you can link it again.")
                    .color(theme::MUTED_TEXT),
            );
        });
    });
}
