//! "Add contact" window: share your own invite (QR + link) or paste a friend's invite.

use brumm_core::api::Command;

use crate::{
    state::AppState,
    ui::{qr::QrCache, theme},
};

const QR_SIZE: f32 = 220.0;
const WINDOW_WIDTH: f32 = 420.0;

#[derive(Default)]
pub struct AddContactView {
    pub open: bool,
    pasted_link: String,
    qr: QrCache,
}

pub fn show(ui: &mut egui::Ui, state: &mut AppState, view: &mut AddContactView, commands: &mut Vec<Command>) {
    if !view.open {
        return;
    }
    let mut open = view.open;
    egui::Window::new("Add contact")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(WINDOW_WIDTH)
        .show(ui.ctx(), |ui| {
            share_section(ui, state, view, commands);
            ui.separator();
            paste_section(ui, state, view, commands);
        });
    view.open = open;
}

fn share_section(ui: &mut egui::Ui, state: &AppState, view: &mut AddContactView, commands: &mut Vec<Command>) {
    ui.strong("Share your invite");
    ui.label(egui::RichText::new("Each invite works once and expires after 7 days.").small().color(theme::MUTED_TEXT));
    let Some(link) = &state.invite_link else {
        if ui.button("Create invite").clicked() {
            commands.push(Command::CreateInvite);
        }
        return;
    };
    ui.vertical_centered(|ui| {
        if let Some(texture) = view.qr.texture(ui.ctx(), link) {
            ui.add(egui::Image::from_texture(texture).fit_to_exact_size(egui::vec2(QR_SIZE, QR_SIZE)));
        }
    });
    // Selectable fallback for compositors where programmatic clipboard access fails.
    ui.add(egui::TextEdit::singleline(&mut link.as_str()).desired_width(f32::INFINITY));
    ui.horizontal(|ui| {
        if ui.button("Copy link").clicked() {
            ui.ctx().copy_text(link.clone());
        }
        if ui.button("New invite").clicked() {
            commands.push(Command::CreateInvite);
        }
    });
}

fn paste_section(ui: &mut egui::Ui, state: &mut AppState, view: &mut AddContactView, commands: &mut Vec<Command>) {
    ui.strong("Got an invite from a friend?");
    ui.add(
        egui::TextEdit::singleline(&mut view.pasted_link)
            .hint_text("brumm://invite/…")
            .desired_width(f32::INFINITY),
    );
    ui.horizontal(|ui| {
        let can_connect = !view.pasted_link.trim().is_empty() && !state.connecting;
        if ui.add_enabled(can_connect, egui::Button::new("Connect")).clicked() {
            state.connecting = true;
            commands.push(Command::AcceptInvite { link: std::mem::take(&mut view.pasted_link) });
        }
        if state.connecting {
            ui.spinner();
            ui.label("connecting…");
        }
    });
}
