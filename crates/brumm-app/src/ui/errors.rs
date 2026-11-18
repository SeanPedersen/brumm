//! Dismissable error banner at the top of the window.

use crate::{state::AppState, ui::theme};

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    if state.errors.is_empty() {
        return;
    }
    let mut dismissed = None;
    egui::Panel::top("errors")
        .frame(egui::Frame::new().fill(theme::ERROR_FILL).inner_margin(6))
        .show(ui, |ui| {
            for (index, error) in state.errors.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("✕").clicked() {
                        dismissed = Some(index);
                    }
                    ui.label(error);
                });
            }
        });
    if let Some(index) = dismissed {
        state.errors.remove(index);
    }
}
