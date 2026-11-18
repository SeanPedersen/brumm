//! Splash shown until the core reports `Ready`.

pub fn show(ui: &mut egui::Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.centered_and_justified(|ui| {
            ui.spinner();
        });
    });
}
