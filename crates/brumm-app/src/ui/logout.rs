//! "Log out" from the menu: a confirmation, then a blocking "Logging out…" overlay until the
//! core reports `LoggedOut` (it first tells the other devices, then erases this one).

use brumm_core::api::Command;

use crate::{state::AppState, ui::theme};

const DIALOG_WIDTH: f32 = 340.0;

#[derive(Default)]
pub struct LogoutView {
    pub confirm_open: bool,
}

/// Starts the log-out. Shared by the confirmation and the "device was removed" screen.
pub fn request(state: &mut AppState, commands: &mut Vec<Command>) {
    state.logging_out = true;
    commands.push(Command::LogOut);
}

pub fn show(
    ui: &egui::Ui,
    state: &mut AppState,
    view: &mut LogoutView,
    commands: &mut Vec<Command>,
) {
    if state.logging_out {
        progress(ui);
        return;
    }
    if !view.confirm_open {
        return;
    }
    let only_device = state.devices.len() <= 1;
    let (mut confirmed, mut cancelled) = (false, false);
    egui::Modal::new(egui::Id::unique("logout_confirm")).show(ui.ctx(), |ui| {
        ui.set_width(DIALOG_WIDTH);
        ui.heading("Log out of this device?");
        ui.add_space(8.0);
        ui.label("brumm deletes your chats, contacts and keys from this device.");
        ui.add_space(4.0);
        if only_device {
            ui.label(
                egui::RichText::new(
                    "This is your only device. Without your 24-word seed phrase, this account \
                     cannot be restored, and your chat history is gone for good.",
                )
                .color(theme::DANGER),
            );
        } else {
            ui.label(
                egui::RichText::new(
                    "Your other devices are told to remove it and keep everything. \
                     Link this device again to come back.",
                )
                .color(theme::MUTED_TEXT),
            );
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                confirmed = ui.add(theme::danger_button("Log out")).clicked();
                cancelled = ui.button("Cancel").clicked();
            });
        });
    });
    if confirmed {
        view.confirm_open = false;
        request(state, commands);
    } else if cancelled || ui.input(|input| input.key_pressed(egui::Key::Escape)) {
        view.confirm_open = false;
    }
}

fn progress(ui: &egui::Ui) {
    egui::Modal::new(egui::Id::unique("logout_progress")).show(ui.ctx(), |ui| {
        ui.set_width(DIALOG_WIDTH);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Logging out… telling your other devices, then erasing this one.");
        });
    });
}
