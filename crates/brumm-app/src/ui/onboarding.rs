//! First-run screen: create a new identity, restore one from a seed phrase, or link this
//! device to an identity that already runs on another device.

use brumm_core::api::Command;

use crate::ui::{qr::QrCache, theme};

const FORM_WIDTH: f32 = 380.0;
const QR_SIZE: f32 = 220.0;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Mode {
    #[default]
    Create,
    Link,
    Restore,
}

#[derive(Default)]
pub struct OnboardingView {
    mode: Mode,
    display_name: String,
    seed_phrase: String,
    submitted: bool,
    linking_requested: bool,
    qr: QrCache,
}

impl OnboardingView {
    /// Re-enables the submit button after the core rejected the attempt.
    pub fn allow_retry(&mut self) {
        self.submitted = false;
    }
}

pub fn show(ui: &mut egui::Ui, view: &mut OnboardingView, link_ticket: Option<&str>, commands: &mut Vec<Command>) {
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.heading(egui::RichText::new("brumm").size(48.0).color(theme::ACCENT));
                ui.label(egui::RichText::new("no phone number · no servers · just you").color(theme::MUTED_TEXT));
                ui.add_space(24.0);
                ui.allocate_ui(egui::vec2(FORM_WIDTH, 0.0), |ui| {
                    mode_tabs(ui, view);
                    ui.add_space(12.0);
                    match view.mode {
                        Mode::Create | Mode::Restore => identity_form(ui, view, commands),
                        Mode::Link => link_panel(ui, view, link_ticket, commands),
                    }
                });
            });
        });
    });
}

fn mode_tabs(ui: &mut egui::Ui, view: &mut OnboardingView) {
    ui.horizontal(|ui| {
        ui.selectable_value(&mut view.mode, Mode::Create, "I'm new");
        ui.selectable_value(&mut view.mode, Mode::Link, "I use brumm on another device");
        ui.selectable_value(&mut view.mode, Mode::Restore, "Restore");
    });
}

fn identity_form(ui: &mut egui::Ui, view: &mut OnboardingView, commands: &mut Vec<Command>) {
    let restore = view.mode == Mode::Restore;
    ui.label("What should your contacts call you?");
    ui.add(egui::TextEdit::singleline(&mut view.display_name).hint_text("display name").desired_width(f32::INFINITY));
    if restore {
        ui.label("Your 24-word seed phrase");
        ui.add(
            egui::TextEdit::multiline(&mut view.seed_phrase)
                .hint_text("word1 word2 …")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        ui.label(egui::RichText::new("Restoring starts fresh: contacts and chats are not recovered. To keep them, link from a device you still have.").small().color(theme::MUTED_TEXT));
    }
    ui.add_space(8.0);
    let ready = !view.display_name.trim().is_empty() && (!restore || !view.seed_phrase.trim().is_empty());
    let label = if restore { "Restore identity" } else { "Let's go" };
    let button = ui.add_enabled(ready && !view.submitted, egui::Button::new(label).min_size(egui::vec2(FORM_WIDTH, 32.0)));
    if button.clicked() {
        view.submitted = true;
        let display_name = view.display_name.trim().to_owned();
        commands.push(if restore {
            Command::RestoreIdentity { phrase: view.seed_phrase.clone(), display_name }
        } else {
            Command::CreateIdentity { display_name }
        });
    }
    if view.submitted {
        ui.spinner();
    }
}

fn link_panel(ui: &mut egui::Ui, view: &mut OnboardingView, link_ticket: Option<&str>, commands: &mut Vec<Command>) {
    if !view.linking_requested {
        view.linking_requested = true;
        commands.push(Command::StartLinking);
    }
    ui.label("On your other device, open “Devices” → “Link a new device” and paste this link (or scan the code with a phone, later).");
    ui.add_space(8.0);
    let Some(link) = link_ticket else {
        ui.spinner();
        return;
    };
    ui.vertical_centered(|ui| {
        if let Some(texture) = view.qr.texture(ui.ctx(), link) {
            ui.add(egui::Image::from_texture(texture).fit_to_exact_size(egui::vec2(QR_SIZE, QR_SIZE)));
        }
    });
    ui.add(egui::TextEdit::singleline(&mut link.to_owned()).desired_width(f32::INFINITY));
    if ui.button("Copy link").clicked() {
        ui.ctx().copy_text(link.to_owned());
    }
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(egui::RichText::new("waiting for your other device…").color(theme::MUTED_TEXT));
    });
}
