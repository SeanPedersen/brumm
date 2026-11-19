//! Left panel: search + menu bar and the chat list (avatar, name, last message, time, unread badge).

use brumm_core::api::{Command, ContactView, Direction, MessageView};
use egui::{Align2, FontId, Rect, Sense, Vec2};

use crate::{
    state::AppState,
    ui::{ViewState, avatar, theme, time_fmt},
};

const ROW_PADDING: f32 = 12.0;
const ROW_TEXT_GAP: f32 = 12.0;
const NAME_FONT_SIZE: f32 = 16.0;
const PREVIEW_FONT_SIZE: f32 = 14.0;
const TIME_FONT_SIZE: f32 = 12.0;
const BADGE_HEIGHT: f32 = 20.0;
const BADGE_MIN_WIDTH: f32 = 20.0;
const BADGE_PADDING: f32 = 12.0;
const MENU_BUTTON_SIZE: f32 = 36.0;
const MAX_BADGE_COUNT: usize = 99;
const OWN_MESSAGE_PREFIX: &str = "You: ";

pub fn show(ui: &mut egui::Ui, state: &mut AppState, view: &mut ViewState, commands: &mut Vec<Command>) {
    egui::Panel::left("contacts")
        .resizable(true)
        .default_size(theme::SIDEBAR_WIDTH)
        .size_range(theme::SIDEBAR_MIN_WIDTH..=theme::SIDEBAR_MAX_WIDTH)
        .frame(egui::Frame::new().fill(theme::SIDEBAR_BG))
        .show(ui, |ui| {
            top_bar(ui, state, view);
            let clicked = chat_list(ui, state, &view.search);
            if let Some(contact) = clicked {
                state.select(contact, ui.ctx());
                commands.push(Command::LoadConversation { contact });
            }
        });
}

fn top_bar(ui: &mut egui::Ui, state: &AppState, view: &mut ViewState) {
    egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            menu_button(ui, state, view);
            // Right-to-left so the search field takes exactly the space the "+" button leaves: a
            // hand-computed width can overflow and push the panel's minimum width up, which makes
            // a dragged sidebar width creep or snap back.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let add = ui.add_sized(
                    [MENU_BUTTON_SIZE, MENU_BUTTON_SIZE],
                    egui::Button::new(egui::RichText::new("+").size(22.0).color(theme::ACCENT_LIGHT))
                        .fill(egui::Color32::TRANSPARENT)
                        .corner_radius(egui::CornerRadius::same(18)),
                );
                if add.on_hover_text("Add contact").clicked() {
                    view.add_contact.open = true;
                }
                search_field(ui, &mut view.search);
            });
        });
    });
}

fn search_field(ui: &mut egui::Ui, search: &mut String) {
    ui.scope(|ui| {
        let visuals = &mut ui.style_mut().visuals;
        visuals.widgets.inactive.corner_radius = theme::INPUT_RADIUS;
        visuals.widgets.hovered.corner_radius = theme::INPUT_RADIUS;
        visuals.widgets.active.corner_radius = theme::INPUT_RADIUS;
        visuals.selection.stroke = egui::Stroke::new(1.0, theme::ACCENT);
        ui.add(
            egui::TextEdit::singleline(search)
                .hint_text("Search")
                .margin(egui::Margin::symmetric(14, 8))
                .desired_width(ui.available_width()),
        );
    });
}

fn menu_button(ui: &mut egui::Ui, state: &AppState, view: &mut ViewState) {
    ui.scope(|ui| {
        // Keep the button exactly MENU_BUTTON_SIZE wide: the search field's width is derived from it,
        // and any overflow would force the panel's minimum width up so it snaps back after resizing.
        ui.spacing_mut().button_padding = Vec2::ZERO;
        ui.set_min_size(Vec2::splat(MENU_BUTTON_SIZE));
        ui.set_max_width(MENU_BUTTON_SIZE);
        ui.menu_button(egui::RichText::new("☰").size(20.0).color(theme::MUTED_TEXT), |ui| {
            ui.set_min_width(220.0);
            if let Some(profile) = &state.profile {
                ui.horizontal(|ui| {
                    avatar::show(ui, theme::HEADER_AVATAR_SIZE, &profile.user_key, &profile.display_name, state.avatars.get(&profile.user_key));
                    ui.vertical(|ui| {
                        ui.strong(&profile.display_name);
                        ui.label(egui::RichText::new(&profile.fingerprint).monospace().small().color(theme::MUTED_TEXT))
                            .on_hover_text("Your fingerprint — compare it with contacts to verify each other");
                    });
                });
                ui.separator();
            }
            if ui.button("Set profile photo…").clicked() {
                view.chat.dialogs.pick_avatar(ui.ctx());
                ui.close();
            }
            if state.profile.as_ref().is_some_and(|profile| state.avatars.contains_key(&profile.user_key))
                && ui.button("Remove profile photo").clicked()
            {
                view.remove_avatar_requested = true;
                ui.close();
            }
            ui.separator();
            if ui.button("Add contact").clicked() {
                view.add_contact.open = true;
                ui.close();
            }
            let devices = format!("Devices ({})", state.devices.len());
            if ui.button(devices).clicked() {
                view.devices.open = true;
                ui.close();
            }
        });
    });
}

/// Returns the contact the user clicked, if any.
fn chat_list(ui: &mut egui::Ui, state: &AppState, search: &str) -> Option<brumm_core::identity::UserKey> {
    let contacts = visible_contacts(state, search);
    if contacts.is_empty() {
        let hint = if state.contacts.is_empty() { "No chats yet" } else { "Nothing found" };
        ui.add_space(32.0);
        ui.vertical_centered(|ui| ui.label(egui::RichText::new(hint).color(theme::MUTED_TEXT)));
        return None;
    }
    let mut clicked = None;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for contact in contacts {
            if contact_row(ui, state, contact) {
                clicked = Some(contact.user_key);
            }
        }
    });
    clicked
}

/// Filtered by the search text and ordered by most recent activity first.
fn visible_contacts<'a>(state: &'a AppState, search: &str) -> Vec<&'a ContactView> {
    let needle = search.trim().to_lowercase();
    let mut contacts: Vec<&ContactView> = state
        .contacts
        .iter()
        .filter(|contact| needle.is_empty() || contact.display_name.to_lowercase().contains(&needle))
        .collect();
    contacts.sort_by_key(|contact| std::cmp::Reverse(last_activity_ms(state, contact)));
    contacts
}

fn last_message<'a>(state: &'a AppState, contact: &ContactView) -> Option<&'a MessageView> {
    state.previews.get(&contact.user_key)
}

fn last_activity_ms(state: &AppState, contact: &ContactView) -> u64 {
    last_message(state, contact).map_or(contact.added_at_ms, |message| message.sent_at_ms)
}

/// Paints one chat-list row; returns whether it was clicked.
fn contact_row(ui: &mut egui::Ui, state: &AppState, contact: &ContactView) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), theme::CONTACT_ROW_HEIGHT), Sense::click());
    let selected = state.selected == Some(contact.user_key);
    let background = if selected {
        theme::ROW_SELECTED
    } else if response.hovered() {
        theme::ROW_HOVER
    } else {
        theme::SIDEBAR_BG
    };
    ui.painter().rect_filled(rect, 0.0, background);
    if response.hovered() {
        ui.set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let avatar_center = egui::pos2(rect.left() + ROW_PADDING + theme::CONTACT_AVATAR_SIZE / 2.0, rect.center().y);
    avatar::paint(ui, avatar_center, theme::CONTACT_AVATAR_SIZE, &contact.user_key, &contact.display_name, notes_aware_avatar(state, &contact.user_key));

    let secondary = if selected { theme::MUTED_TEXT_ON_SELECTED } else { theme::MUTED_TEXT };
    let text_left = avatar_center.x + theme::CONTACT_AVATAR_SIZE / 2.0 + ROW_TEXT_GAP;
    let text_right = rect.right() - ROW_PADDING;
    let top_line_y = rect.top() + rect.height() * 0.3;
    let bottom_line_y = rect.top() + rect.height() * 0.7;

    let last = last_message(state, contact);
    let stamp_width = match last {
        Some(message) => paint_right(ui, text_right, top_line_y, &time_fmt::list_stamp(message.sent_at_ms), secondary),
        None => 0.0,
    };
    let name_right = text_right - stamp_width - ROW_TEXT_GAP;
    paint_clipped(ui, text_left, name_right, top_line_y, &contact.display_name, FontId::proportional(NAME_FONT_SIZE), theme::TEXT);

    let unread = state.unread.get(&contact.user_key).copied().unwrap_or(0);
    let badge_width = if unread > 0 { paint_badge(ui, text_right, bottom_line_y, unread) } else { 0.0 };
    let preview_right = text_right - badge_width - if badge_width > 0.0 { ROW_TEXT_GAP } else { 0.0 };
    let preview = preview_text(last);
    paint_clipped(ui, text_left, preview_right, bottom_line_y, &preview, FontId::proportional(PREVIEW_FONT_SIZE), secondary);

    response.clicked()
}

/// Notes show an initial, not our own profile photo.
fn notes_aware_avatar<'a>(state: &'a AppState, key: &brumm_core::identity::UserKey) -> Option<&'a crate::state::AvatarImage> {
    if state.is_notes(key) { None } else { state.avatars.get(key) }
}

fn preview_text(last: Option<&MessageView>) -> String {
    let Some(message) = last else { return String::new() };
    let first_line = message.text.lines().next().unwrap_or_default();
    match message.direction {
        Direction::Outgoing => format!("{OWN_MESSAGE_PREFIX}{first_line}"),
        Direction::Incoming => first_line.to_owned(),
    }
}

/// Paints right-aligned text and returns its width.
fn paint_right(ui: &egui::Ui, right: f32, center_y: f32, text: &str, color: egui::Color32) -> f32 {
    let rect = ui.painter().text(
        egui::pos2(right, center_y),
        Align2::RIGHT_CENTER,
        text,
        FontId::proportional(TIME_FONT_SIZE),
        color,
    );
    rect.width()
}

/// Paints left-aligned text, cut off at `right` instead of overflowing into neighbours.
fn paint_clipped(ui: &egui::Ui, left: f32, right: f32, center_y: f32, text: &str, font: FontId, color: egui::Color32) {
    if right <= left {
        return;
    }
    let clip = Rect::from_min_max(
        egui::pos2(left, center_y - theme::CONTACT_ROW_HEIGHT / 2.0),
        egui::pos2(right, center_y + theme::CONTACT_ROW_HEIGHT / 2.0),
    );
    ui.painter().with_clip_rect(clip).text(egui::pos2(left, center_y), Align2::LEFT_CENTER, text, font, color);
}

/// Paints the unread pill at the row's right edge and returns its width.
fn paint_badge(ui: &egui::Ui, right: f32, center_y: f32, unread: usize) -> f32 {
    let label = if unread > MAX_BADGE_COUNT { format!("{MAX_BADGE_COUNT}+") } else { unread.to_string() };
    let font = FontId::proportional(TIME_FONT_SIZE);
    let galley = ui.painter().layout_no_wrap(label, font, egui::Color32::WHITE);
    let width = (galley.size().x + BADGE_PADDING).max(BADGE_MIN_WIDTH);
    let rect = Rect::from_center_size(egui::pos2(right - width / 2.0, center_y), egui::vec2(width, BADGE_HEIGHT));
    ui.painter().rect_filled(rect, BADGE_HEIGHT / 2.0, theme::ACCENT);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, egui::Color32::WHITE);
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, events: Vec<egui::Event>, state: &mut AppState, view: &mut ViewState) -> f32 {
        let screen_rect = Some(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(960.0, 680.0)));
        let input = egui::RawInput { screen_rect, events, ..Default::default() };
        let mut width = 0.0;
        let mut output = ctx.run_ui(input, |ui| {
            let mut commands = Vec::new();
            show(ui, state, view, &mut commands);
            width = ui.available_rect_before_wrap().left();
        });
        output.textures_delta.clear();
        width
    }

    #[test]
    fn dragged_width_sticks() {
        let ctx = egui::Context::default();
        crate::ui::theme::apply(&ctx);
        let (mut state, mut view) = (AppState::default(), ViewState::default());
        let mut widths = Vec::new();
        widths.push(frame(&ctx, vec![], &mut state, &mut view));
        widths.push(frame(&ctx, vec![], &mut state, &mut view));
        let edge = egui::pos2(widths[1], 300.0);
        let target = egui::pos2(widths[1] + 100.0, 300.0);
        let button = |pressed| egui::Event::PointerButton {
            pos: if pressed { edge } else { target },
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        widths.push(frame(&ctx, vec![egui::Event::PointerMoved(edge)], &mut state, &mut view));
        widths.push(frame(&ctx, vec![button(true)], &mut state, &mut view));
        widths.push(frame(&ctx, vec![egui::Event::PointerMoved(target)], &mut state, &mut view));
        widths.push(frame(&ctx, vec![egui::Event::PointerMoved(target)], &mut state, &mut view));
        widths.push(frame(&ctx, vec![button(false)], &mut state, &mut view));
        for _ in 0..3 {
            widths.push(frame(&ctx, vec![], &mut state, &mut view));
        }
                let after_release = &widths[widths.len() - 3..];
        assert!(after_release.iter().all(|width| (*width - (widths[1] + 100.0)).abs() < 1.0), "{widths:?}");
    }
}
