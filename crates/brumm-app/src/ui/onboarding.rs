//! First-run screen: create a new identity or restore one from a seed phrase.

use brumm_core::api::Command;

use crate::ui::theme;

const FORM_WIDTH: f32 = 360.0;

#[derive(Default)]
pub struct OnboardingView {
    display_name: String,
    restore: bool,
    seed_phrase: String,
    submitted: bool,
}

impl OnboardingView {
    /// Re-enables the submit button after the core rejected the attempt.
    pub fn allow_retry(&mut self) {
        self.submitted = false;
    }
}

pub fn show(ui: &mut egui::Ui, view: &mut OnboardingView, commands: &mut Vec<Command>) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.15);
            ui.heading(egui::RichText::new("brumm").size(48.0).color(theme::ACCENT));
            ui.label(egui::RichText::new("no phone number · no servers · just you").color(theme::MUTED_TEXT));
            ui.add_space(24.0);

            ui.allocate_ui(egui::vec2(FORM_WIDTH, 0.0), |ui| {
                ui.label("What should your contacts call you?");
                ui.add(egui::TextEdit::singleline(&mut view.display_name).hint_text("display name").desired_width(f32::INFINITY));
                ui.checkbox(&mut view.restore, "Restore an existing identity from my seed phrase");
                if view.restore {
                    ui.add(
                        egui::TextEdit::multiline(&mut view.seed_phrase)
                            .hint_text("24 words separated by spaces")
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                }
                ui.add_space(8.0);
                let ready = !view.display_name.trim().is_empty() && (!view.restore || !view.seed_phrase.trim().is_empty());
                let label = if view.restore { "Restore identity" } else { "Create identity" };
                let button = ui.add_enabled(ready && !view.submitted, egui::Button::new(label).min_size(egui::vec2(FORM_WIDTH, 32.0)));
                if button.clicked() {
                    view.submitted = true;
                    commands.push(command(view));
                }
                if view.submitted {
                    ui.spinner();
                }
            });
        });
    });
}

fn command(view: &OnboardingView) -> Command {
    let display_name = view.display_name.trim().to_owned();
    if view.restore {
        Command::RestoreIdentity { phrase: view.seed_phrase.clone(), display_name }
    } else {
        Command::CreateIdentity { display_name }
    }
}
