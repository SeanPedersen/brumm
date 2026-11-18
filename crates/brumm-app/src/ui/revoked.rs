//! Shown when another device of ours removed this one from the identity.

use crate::ui::theme;

pub fn show(ui: &mut egui::Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.heading(egui::RichText::new("This device was removed").color(theme::ACCENT));
            ui.label("It no longer sends or receives messages for your account.");
            ui.label(egui::RichText::new("To use brumm here again, delete this profile's data folder and link it anew.").color(theme::MUTED_TEXT));
        });
    });
}
