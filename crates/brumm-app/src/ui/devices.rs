//! "Devices" window: this identity's linked devices, removing one, and linking a new device
//! by pasting the link it shows.

use brumm_core::api::{Command, DeviceId};

use crate::{state::AppState, ui::theme};

const WINDOW_WIDTH: f32 = 420.0;

#[derive(Default)]
pub struct DevicesView {
    pub open: bool,
    pasted_link: String,
    confirm_remove: Option<DeviceId>,
}

pub fn show(ui: &mut egui::Ui, state: &AppState, view: &mut DevicesView, commands: &mut Vec<Command>) {
    if !view.open {
        return;
    }
    let mut open = view.open;
    egui::Window::new("Your devices")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(WINDOW_WIDTH)
        .show(ui.ctx(), |ui| {
            device_list(ui, state, view, commands);
            ui.separator();
            link_section(ui, view, commands);
        });
    view.open = open;
}

fn device_list(ui: &mut egui::Ui, state: &AppState, view: &mut DevicesView, commands: &mut Vec<Command>) {
    for device in &state.devices {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&device.label).strong());
            ui.label(egui::RichText::new(short_id(&device.device_id)).monospace().small().color(theme::MUTED_TEXT));
            if device.this_device {
                ui.label(egui::RichText::new("this device").small().color(theme::ACCENT));
                return;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if view.confirm_remove == Some(device.device_id) {
                    if ui.button(egui::RichText::new("Yes, remove").color(theme::DANGER)).clicked() {
                        commands.push(Command::RemoveDevice { device_id: device.device_id });
                        view.confirm_remove = None;
                    }
                    if ui.button("Cancel").clicked() {
                        view.confirm_remove = None;
                    }
                } else if ui.button("Remove").clicked() {
                    view.confirm_remove = Some(device.device_id);
                }
            });
        });
    }
}

fn link_section(ui: &mut egui::Ui, view: &mut DevicesView, commands: &mut Vec<Command>) {
    ui.strong("Link a new device");
    ui.label(egui::RichText::new("On the new device choose “I use brumm on another device” and paste its link here. It gets your contacts and the last 60 days of chats.").small().color(theme::MUTED_TEXT));
    ui.add(egui::TextEdit::singleline(&mut view.pasted_link).hint_text("brumm://link/…").desired_width(f32::INFINITY));
    let can_link = view.pasted_link.trim().starts_with("brumm://link/");
    if ui.add_enabled(can_link, theme::primary_button("Link device")).clicked() {
        commands.push(Command::LinkDevice { link: std::mem::take(&mut view.pasted_link) });
    }
}

fn short_id(device_id: &DeviceId) -> String {
    device_id[..3].iter().map(|byte| format!("{byte:02x}")).collect()
}
