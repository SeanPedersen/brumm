//! Main layout: contact sidebar on the left, the selected chat in the center, dialogs on top.
//! Also turns finished file dialogs and dropped files into core commands.

use brumm_core::{api::Command, identity::UserKey};

use crate::{
    state::AppState,
    ui::{
        ViewState, add_contact, chat, composer, devices, files, logout, narrow::Mode, seed_phrase,
        sidebar, theme,
    },
};

pub fn show(
    ui: &mut egui::Ui,
    state: &mut AppState,
    view: &mut ViewState,
    commands: &mut Vec<Command>,
) {
    handle_file_events(ui, state, view, commands);
    let search_active = view.chat.search.is_active();
    if view.narrow.begin_frame(ui.available_width(), search_active) {
        view.chat.search.close();
    }
    let mode = view
        .narrow
        .mode(state.selected.is_some(), view.chat.search.is_active());
    if mode != Mode::Chat {
        sidebar::show(ui, state, view, commands, mode == Mode::List);
    }
    if mode != Mode::List {
        chat_pane(ui, state, view, commands, mode == Mode::Chat);
    }
    add_contact::show(ui, state, &mut view.add_contact, commands);
    devices::show(ui, state, &mut view.devices, commands);
    seed_phrase::show(ui, state);
    logout::show(ui, state, &mut view.logout, commands);
}

fn chat_pane(
    ui: &mut egui::Ui,
    state: &mut AppState,
    view: &mut ViewState,
    commands: &mut Vec<Command>,
    narrow: bool,
) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new())
        .show(ui, |ui| match state.selected {
            Some(contact) => {
                let draft = view.drafts.entry(contact).or_default();
                let back = chat::show(ui, state, &mut view.chat, contact, draft, commands, narrow);
                drop_overlay(ui);
                if back {
                    view.chat.search.close();
                    view.narrow.back();
                }
            }
            None => empty_state(ui),
        });
}

fn handle_file_events(
    ui: &egui::Ui,
    state: &mut AppState,
    view: &mut ViewState,
    commands: &mut Vec<Command>,
) {
    if std::mem::take(&mut view.remove_avatar_requested) {
        commands.push(Command::SetAvatar { image: Vec::new() });
    }
    let mut picked = view.chat.dialogs.poll();
    if let Some(contact) = state.selected {
        let dropped = ui.input(|input| input.raw.dropped_files.clone());
        for file in dropped {
            view.chat
                .dialogs
                .take_dropped(ui.ctx(), contact, file, &mut picked);
        }
        let menu_paste = ui
            .data_mut(|data| data.remove_temp::<bool>(composer::paste_menu_requested_id()))
            .unwrap_or(false);
        let items = if paste_key_pressed(ui) || menu_paste {
            files::clipboard_items()
        } else {
            Vec::new()
        };
        if menu_paste {
            tracing::info!(found = items.len(), "paste requested from context menu");
        }
        if menu_paste && items.is_empty() {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        }
        for item in items {
            match item {
                files::ClipboardItem::File(path) => {
                    picked.push(files::Picked::Attachment { contact, path })
                }
                files::ClipboardItem::Image(bytes) => match files::attachment_from_encoded(bytes) {
                    Ok((name, data)) => commands.push(Command::SendFile {
                        contact,
                        name,
                        data,
                    }),
                    Err(message) => state.errors.push(message),
                },
            }
        }
        for image in pasted_images(ui) {
            match files::attachment_from_pixels(&image) {
                Ok((name, data)) => commands.push(Command::SendFile {
                    contact,
                    name,
                    data,
                }),
                Err(message) => state.errors.push(message),
            }
        }
    }
    for item in picked {
        match item {
            files::Picked::Attachment { contact, path } => {
                send_file(state, contact, &path, commands)
            }
            files::Picked::Avatar(path) => match files::avatar_from_file(&path) {
                Ok(image) => commands.push(Command::SetAvatar { image }),
                Err(message) => state.errors.push(message),
            },
            files::Picked::AvatarData(bytes) => match files::avatar_from_bytes(&bytes) {
                Ok(image) => commands.push(Command::SetAvatar { image }),
                Err(message) => state.errors.push(message),
            },
            files::Picked::SaveAs { contact, id, path } => {
                commands.push(Command::SaveAttachment { contact, id, path })
            }
            files::Picked::Dropped {
                contact,
                name,
                data,
            } => match files::attachment_from_dropped(name, &data) {
                Ok((name, data)) => commands.push(Command::SendFile {
                    contact,
                    name,
                    data,
                }),
                Err(message) => state.errors.push(message),
            },
            files::Picked::Unavailable(message) => state.errors.push(message.to_owned()),
        }
    }
}

/// Images pasted with Ctrl+V (egui-winit only emits these when the clipboard holds no text).
fn pasted_images(ui: &egui::Ui) -> Vec<std::sync::Arc<egui::ColorImage>> {
    ui.input(|input| {
        input
            .events
            .iter()
            .filter_map(|event| match event {
                egui::Event::PasteImage(image) => Some(image.clone()),
                _ => None,
            })
            .collect()
    })
}

/// Ctrl+V reaches the UI as a key event only when the clipboard has neither text nor image data.
fn paste_key_pressed(ui: &egui::Ui) -> bool {
    ui.input(|input| input.modifiers.command_only() && input.key_pressed(egui::Key::V))
}

fn send_file(
    state: &mut AppState,
    contact: UserKey,
    path: &std::path::Path,
    commands: &mut Vec<Command>,
) {
    match files::read_attachment(path) {
        Ok((name, data)) => commands.push(Command::SendFile {
            contact,
            name,
            data,
        }),
        Err(message) => state.errors.push(message),
    }
}

/// "Drop file to send" hint while a file is dragged over the window.
fn drop_overlay(ui: &egui::Ui) {
    if ui.input(|input| input.raw.hovered_files.is_empty()) {
        return;
    }
    let screen = ui.ctx().content_rect();
    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::unique("drop_overlay"),
    ));
    painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));
    painter.text(
        screen.center(),
        egui::Align2::CENTER_CENTER,
        "Drop a file to send it",
        egui::FontId::proportional(22.0),
        theme::TEXT,
    );
}

fn empty_state(ui: &mut egui::Ui) {
    egui::Frame::new().fill(theme::CHAT_BG).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        let center = ui.max_rect().center();
        let galley = ui.painter().layout_no_wrap(
            "Select a chat to start messaging".to_owned(),
            egui::FontId::proportional(14.0),
            egui::Color32::WHITE,
        );
        let pill = egui::Rect::from_center_size(center, galley.size() + egui::vec2(24.0, 10.0));
        ui.painter()
            .rect_filled(pill, pill.height() / 2.0, theme::SERVICE_PILL);
        ui.painter().galley(
            pill.center() - galley.size() / 2.0,
            galley,
            egui::Color32::WHITE,
        );
    });
}

#[cfg(test)]
mod tests {
    use brumm_core::api::{
        AttachmentView, ContactView, DeliveryStatus, Direction, MessageView, Trust,
    };

    use super::*;
    use crate::state::LoadedAttachment;

    fn message(
        index: u8,
        direction: Direction,
        text: &str,
        attachment: Option<AttachmentView>,
    ) -> MessageView {
        MessageView {
            id: [index; 16],
            contact: [1; 32],
            direction,
            text: text.to_owned(),
            attachment,
            reactions: Default::default(),
            sent_at_ms: 1_700_000_000_000 + u64::from(index) * 3_600_000,
            status: DeliveryStatus::Delivered,
        }
    }

    fn png_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        let pixels = image::RgbImage::from_pixel(64, 48, image::Rgb([200, 80, 40]));
        pixels
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encode test png");
        bytes
    }

    /// Renders text, file and image messages in selection mode without panicking.
    #[test]
    fn renders_every_message_kind() {
        let ctx = egui::Context::default();
        crate::ui::theme::apply(&ctx);
        egui_extras::install_image_loaders(&ctx);
        let contact_key = [1; 32];
        let mut state = AppState {
            ready: true,
            ..Default::default()
        };
        state.contacts.push(ContactView {
            user_key: contact_key,
            display_name: "Bob".into(),
            fingerprint: "ab".into(),
            trust: Trust::Verified,
            added_at_ms: 0,
        });
        state.selected = Some(contact_key);
        let image = AttachmentView {
            name: "pic.png".into(),
            size: 100,
        };
        let file = AttachmentView {
            name: "a-very-long-document-name-that-must-be-truncated.pdf".into(),
            size: 3_000_000,
        };
        state.conversations.insert(
            contact_key,
            vec![
                message(1, Direction::Incoming, "hello there, this is a fairly long line of text that should wrap around the bubble", None),
                message(2, Direction::Outgoing, "short", None),
                message(3, Direction::Outgoing, "", Some(file)),
                message(4, Direction::Incoming, "", Some(image)),
            ],
        );
        let bytes = png_bytes();
        state.attachments.insert(
            (contact_key, [4; 16]),
            LoadedAttachment {
                bytes: bytes.into(),
                image_size: Some((64, 48)),
            },
        );
        let mut view = ViewState::default();
        view.chat.select_for_test([1; 16]);
        for _ in 0..3 {
            let screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 680.0),
            ));
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect,
                    ..Default::default()
                },
                |ui| {
                    let mut commands = Vec::new();
                    show(ui, &mut state, &mut view, &mut commands);
                },
            );
            output.textures_delta.clear();
        }
    }
}
