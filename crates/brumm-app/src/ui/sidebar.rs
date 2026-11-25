//! Left panel: search + menu bar and the chat list (avatar, name, last message, time, unread badge).
//! While a room search is active, the same field and list show that room's messages instead.

use brumm_core::{
    api::{Command, ConversationId, Direction, GroupInvitationView, InvitationStage, MessageView},
    wire::MessageId,
};
use egui::{Align2, FontId, Rect, Sense, Vec2};

use crate::{
    state::{AppState, ChatTitle},
    ui::{ViewState, avatar, chat_search, icons, theme, time_fmt},
};

const ROW_PADDING: f32 = 12.0;
/// Inset of the search row from the sidebar edge.
const TOP_BAR_PAD_X: i8 = 4;
const TOP_BAR_PAD_Y: i8 = 8;
/// Gap between a side button and the search pill.
const TOP_BAR_GAP: f32 = 4.0;
/// Hit target for the menu, add, and close glyphs. Clamped so theme button padding
/// cannot widen the slot and leave a band of empty space beside the pill.
const TOP_BAR_BUTTON: f32 = 28.0;
const SEARCH_MARGIN_X: i8 = 12;
const SEARCH_MARGIN_Y: i8 = 8;
const ROW_TEXT_GAP: f32 = 12.0;
const NAME_FONT_SIZE: f32 = 16.0;
const PREVIEW_FONT_SIZE: f32 = 14.0;
const TIME_FONT_SIZE: f32 = 12.0;
const BADGE_HEIGHT: f32 = 20.0;
const BADGE_MIN_WIDTH: f32 = 20.0;
const BADGE_PADDING: f32 = 12.0;
const MAX_BADGE_COUNT: usize = 99;
const OWN_MESSAGE_PREFIX: &str = "You: ";
const SEARCH_SENDER_YOU: &str = "You";
const INVITATION_PAD_Y: i8 = 8;
/// Clock / spinner in front of an accepted invitation's status line.
const INVITATION_STATUS_ICON: f32 = 14.0;

#[cfg(test)]
thread_local! {
    static SEARCH_RECT: std::cell::Cell<Option<Rect>> = const { std::cell::Cell::new(None) };
    static MENU_RECT: std::cell::Cell<Option<Rect>> = const { std::cell::Cell::new(None) };
    static END_RECT: std::cell::Cell<Option<Rect>> = const { std::cell::Cell::new(None) };
}

/// `full_width`: the narrow layout, where the list is the whole screen instead of a side panel.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut AppState,
    view: &mut ViewState,
    commands: &mut Vec<Command>,
    full_width: bool,
) {
    let frame = egui::Frame::new().fill(theme::SIDEBAR_BG);
    let content = |ui: &mut egui::Ui| {
        view.chat.search.note_open_chat(state.selected);
        top_bar(ui, state, view);
        if view.chat.search.is_active() {
            if let Some(conversation) = state.selected {
                let jumped = message_results(ui, state, view, conversation);
                if let Some(id) = jumped {
                    view.chat.search.reveal(id);
                    view.narrow.show_chat_after_search();
                }
            }
        } else {
            let clicked = chat_list(ui, state, &view.search, commands);
            if let Some(conversation) = clicked {
                state.select(conversation, ui.ctx());
                view.narrow.open_chat();
                commands.push(state.load_command(conversation));
            }
        }
    };
    if full_width {
        egui::CentralPanel::default().frame(frame).show(ui, content);
        return;
    }
    egui::Panel::left("contacts")
        .resizable(true)
        .default_size(theme::SIDEBAR_WIDTH)
        .size_range(theme::SIDEBAR_MIN_WIDTH..=theme::SIDEBAR_MAX_WIDTH)
        .frame(frame)
        .show(ui, content);
}

fn top_bar(ui: &mut egui::Ui, state: &AppState, view: &mut ViewState) {
    let searching = view.chat.search.is_active();
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(TOP_BAR_PAD_X, TOP_BAR_PAD_Y))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = TOP_BAR_GAP;
                let menu = icon_slot(ui, |ui| menu_button(ui, state, view));
                // Right-to-left so the field takes exactly the space the end button leaves.
                // A hand-computed width can overflow and push the panel's minimum up, which
                // makes a dragged sidebar width creep or snap back.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let end = icon_slot(ui, |ui| {
                        if searching {
                            if icon_button(ui, "✕", 18.0, theme::MUTED_TEXT)
                                .on_hover_text("Close search")
                                .clicked()
                            {
                                view.chat.search.close();
                            }
                        } else if icon_button(ui, "+", 22.0, theme::ACCENT_LIGHT)
                            .on_hover_text("Add contact")
                            .clicked()
                        {
                            view.add_contact.open = true;
                        }
                    });
                    if searching {
                        let focus = view.chat.search.take_focus();
                        search_field(ui, view.chat.search.query_mut(), "Search messages", focus);
                    } else {
                        search_field(ui, &mut view.search, "Search", false);
                    }
                    #[cfg(test)]
                    END_RECT.with(|cell| cell.set(Some(end)));
                    #[cfg(not(test))]
                    let _ = end;
                });
                #[cfg(test)]
                MENU_RECT.with(|cell| cell.set(Some(menu)));
                #[cfg(not(test))]
                let _ = menu;
            });
        });
}

/// A side button exactly [`TOP_BAR_BUTTON`] wide. Theme padding would otherwise grow it.
fn icon_slot(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) -> Rect {
    ui.scope(|ui| {
        ui.spacing_mut().button_padding = Vec2::ZERO;
        ui.set_min_size(Vec2::splat(TOP_BAR_BUTTON));
        ui.set_max_width(TOP_BAR_BUTTON);
        add(ui);
    })
    .response
    .rect
}

fn icon_button_widget(glyph: &str, size: f32, color: egui::Color32) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(glyph).size(size).color(color))
        .fill(egui::Color32::TRANSPARENT)
        .frame(false)
        .min_size(Vec2::splat(TOP_BAR_BUTTON))
}

fn icon_button(ui: &mut egui::Ui, glyph: &str, size: f32, color: egui::Color32) -> egui::Response {
    ui.add(icon_button_widget(glyph, size, color))
}

fn search_field(ui: &mut egui::Ui, search: &mut String, hint: &str, focus: bool) -> egui::Response {
    ui.scope(|ui| {
        let visuals = &mut ui.style_mut().visuals;
        visuals.widgets.inactive.corner_radius = theme::INPUT_RADIUS;
        visuals.widgets.hovered.corner_radius = theme::INPUT_RADIUS;
        visuals.widgets.active.corner_radius = theme::INPUT_RADIUS;
        visuals.selection.stroke = egui::Stroke::new(1.0, theme::ACCENT);
        let response = ui.add(
            egui::TextEdit::singleline(search)
                .hint_text(hint)
                .margin(egui::Margin::symmetric(SEARCH_MARGIN_X, SEARCH_MARGIN_Y))
                .desired_width(ui.available_width()),
        );
        #[cfg(test)]
        SEARCH_RECT.with(|cell| cell.set(Some(response.rect)));
        if focus {
            response.request_focus();
        }
        response
    })
    .inner
}

/// Matches from the open room. A click is the message to reveal in the chat.
fn message_results(
    ui: &mut egui::Ui,
    state: &AppState,
    view: &mut ViewState,
    conversation: ConversationId,
) -> Option<MessageId> {
    let name = state
        .chat_title(&conversation)
        .map(|chat| chat.name.to_owned())
        .unwrap_or_default();
    let messages = state
        .conversations
        .get(&conversation)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let highlighted = view.chat.search.highlighted();
    let query = view.chat.search.trimmed_query().to_owned();
    let listed = view.chat.search.hits(messages);
    let truncated = listed.truncated;
    let hits = listed.matches;
    if query.is_empty() {
        empty_hint(ui, "Type to search this chat");
        return None;
    }
    if hits.is_empty() {
        empty_hint(ui, "No messages found");
        return None;
    }
    let count = if truncated {
        format!("{}+ matches", chat_search::MAX_HITS)
    } else if hits.len() == 1 {
        "1 match".to_owned()
    } else {
        format!("{} matches", hits.len())
    };
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(ROW_PADDING);
        ui.label(egui::RichText::new(count).small().color(theme::MUTED_TEXT));
    });
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .id_salt(("room_search", query))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for hit in hits {
                if result_row(ui, hit, &name, highlighted == Some(hit.id)) {
                    clicked = Some(hit.id);
                }
            }
        });
    clicked
}

fn empty_hint(ui: &mut egui::Ui, hint: &str) {
    ui.add_space(32.0);
    ui.vertical_centered(|ui| ui.label(egui::RichText::new(hint).color(theme::MUTED_TEXT)));
}

fn result_row(
    ui: &mut egui::Ui,
    hit: &chat_search::SearchHit,
    contact_name: &str,
    selected: bool,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), theme::CONTACT_ROW_HEIGHT),
        Sense::click(),
    );
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
    let secondary = if selected {
        theme::MUTED_TEXT_ON_SELECTED
    } else {
        theme::MUTED_TEXT
    };
    let text_left = rect.left() + ROW_PADDING;
    let text_right = rect.right() - ROW_PADDING;
    let top_line_y = rect.top() + rect.height() * 0.32;
    let bottom_line_y = rect.top() + rect.height() * 0.70;
    let sender = match (hit.direction, &hit.sender) {
        (Direction::Outgoing, _) => SEARCH_SENDER_YOU,
        (Direction::Incoming, Some(sender)) => sender,
        (Direction::Incoming, None) => contact_name,
    };
    let stamp_width = paint_right(
        ui,
        text_right,
        top_line_y,
        &time_fmt::list_stamp(hit.sent_at_ms),
        secondary,
    );
    paint_clipped(
        ui,
        text_left,
        text_right - stamp_width - ROW_TEXT_GAP,
        top_line_y,
        sender,
        FontId::proportional(NAME_FONT_SIZE),
        theme::TEXT,
    );
    paint_snippet(ui, text_left, text_right, bottom_line_y, hit, secondary);
    response.clicked()
}

fn paint_snippet(
    ui: &egui::Ui,
    left: f32,
    right: f32,
    center_y: f32,
    hit: &chat_search::SearchHit,
    secondary: egui::Color32,
) {
    if right <= left {
        return;
    }
    let width = right - left;
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let format = |color: egui::Color32| egui::TextFormat {
        font_id: FontId::proportional(PREVIEW_FONT_SIZE),
        color,
        ..Default::default()
    };
    if !hit.before.is_empty() {
        job.append(&hit.before, 0.0, format(secondary));
    }
    job.append(&hit.matched, 0.0, format(theme::ACCENT_LIGHT));
    if !hit.after.is_empty() {
        job.append(&hit.after, 0.0, format(secondary));
    }
    let galley = ui.painter().layout_job(job);
    let clip = Rect::from_min_max(
        egui::pos2(left, center_y - theme::CONTACT_ROW_HEIGHT / 2.0),
        egui::pos2(right, center_y + theme::CONTACT_ROW_HEIGHT / 2.0),
    );
    ui.painter().with_clip_rect(clip).galley(
        egui::pos2(left, center_y - galley.size().y / 2.0),
        galley,
        secondary,
    );
}

/// Top right of the menu: compare it across devices to see they run the same build.
fn version_label(ui: &mut egui::Ui) {
    ui.label(
        egui::RichText::new(crate::version::LABEL)
            .small()
            .color(theme::MUTED_TEXT),
    )
    .on_hover_text("App version · build (commit count - commit)");
}

fn menu_button(ui: &mut egui::Ui, state: &AppState, view: &mut ViewState) {
    egui::menu::MenuButton::from_button(icon_button_widget("☰", 18.0, theme::MUTED_TEXT)).ui(
        ui,
        |ui| {
            ui.set_min_width(220.0);
            if state.profile.is_none() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), version_label);
            }
            if let Some(profile) = &state.profile {
                ui.horizontal(|ui| {
                    avatar::show(
                        ui,
                        theme::HEADER_AVATAR_SIZE,
                        &profile.user_key,
                        &profile.display_name,
                        state.avatars.get(&profile.user_key),
                    );
                    ui.vertical(|ui| {
                        ui.strong(&profile.display_name);
                        ui.label(
                            egui::RichText::new(&profile.fingerprint)
                                .monospace()
                                .small()
                                .color(theme::MUTED_TEXT),
                        )
                        .on_hover_text(
                            "Your fingerprint — compare it with contacts to verify each other",
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), version_label);
                });
                ui.separator();
            }
            if ui.button("Set profile photo…").clicked() {
                view.chat
                    .dialogs
                    .pick_avatar(ui.ctx(), crate::ui::files::AvatarTarget::Profile);
                ui.close();
            }
            if state
                .profile
                .as_ref()
                .is_some_and(|profile| state.avatars.contains_key(&profile.user_key))
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
            if ui.button("New group").clicked() {
                view.new_group.open();
                ui.close();
            }
            let devices = format!("Devices ({})", state.devices.len());
            if ui.button(devices).clicked() {
                view.devices.open = true;
                ui.close();
            }
            ui.separator();
            if ui
                .button(egui::RichText::new("Log out…").color(theme::DANGER))
                .clicked()
            {
                view.logout.confirm_open = true;
                ui.close();
            }
        },
    );
}

/// Returns the chat the user clicked, if any.
fn chat_list(
    ui: &mut egui::Ui,
    state: &AppState,
    search: &str,
    commands: &mut Vec<Command>,
) -> Option<ConversationId> {
    invitation_rows(ui, state, commands);
    let chats = visible_chats(state, search);
    if chats.is_empty() {
        let hint = if state.contacts.is_empty() && state.groups.is_empty() {
            "No chats yet"
        } else {
            "Nothing found"
        };
        ui.add_space(32.0);
        ui.vertical_centered(|ui| ui.label(egui::RichText::new(hint).color(theme::MUTED_TEXT)));
        return None;
    }
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for chat in chats {
                if chat_row(ui, state, &chat) {
                    clicked = Some(chat.conversation);
                }
            }
        });
    clicked
}

/// What an answered invitation is waiting for; `None` while it is still open.
fn invitation_status(invitation: &GroupInvitationView) -> Option<String> {
    match invitation.stage {
        InvitationStage::Open => None,
        InvitationStage::Accepted => Some(format!(
            "Waiting for {} to add you…",
            invitation.inviter_name
        )),
        InvitationStage::Loading => Some("Loading members and recent messages…".to_owned()),
    }
}

/// An accepted invitation: a clock while the inviter has not added us (that can take days, so
/// nothing animates), a spinner while the group loads. Only the wait can be cancelled.
fn invitation_progress(
    ui: &mut egui::Ui,
    invitation: &GroupInvitationView,
    status: String,
    commands: &mut Vec<Command>,
) {
    ui.horizontal(|ui| {
        let icon = egui::vec2(INVITATION_STATUS_ICON, INVITATION_STATUS_ICON);
        if invitation.stage == InvitationStage::Loading {
            ui.add(egui::Spinner::new().size(INVITATION_STATUS_ICON));
        } else {
            let (rect, _) = ui.allocate_exact_size(icon, Sense::hover());
            icons::clock(ui.painter(), rect, theme::MUTED_TEXT);
        }
        ui.label(egui::RichText::new(status).small().color(theme::MUTED_TEXT));
        if invitation.stage == InvitationStage::Accepted && ui.small_button("Cancel").clicked() {
            commands.push(Command::DeclineGroupInvitation {
                group: invitation.group,
            });
        }
    });
}

/// Group invitations above the chat list: open ones with Join / Decline, answered ones with
/// their progress until the group appears in the list.
fn invitation_rows(ui: &mut egui::Ui, state: &AppState, commands: &mut Vec<Command>) {
    for invitation in &state.invitations {
        egui::Frame::new()
            .fill(theme::ROW_HOVER)
            .inner_margin(egui::Margin::symmetric(ROW_PADDING as i8, INVITATION_PAD_Y))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(&invitation.name).strong());
                let members = match invitation.member_count {
                    1 => "1 member".to_owned(),
                    count => format!("{count} members"),
                };
                ui.label(
                    egui::RichText::new(format!(
                        "{} invited you · {members}",
                        invitation.inviter_name
                    ))
                    .small()
                    .color(theme::MUTED_TEXT),
                );
                if let Some(status) = invitation_status(invitation) {
                    invitation_progress(ui, invitation, status, commands);
                    return;
                }
                ui.horizontal(|ui| {
                    let group = invitation.group;
                    if ui.button("Join").clicked() {
                        commands.push(Command::AcceptGroupInvitation { group });
                    }
                    if ui.button("Decline").clicked() {
                        commands.push(Command::DeclineGroupInvitation { group });
                    }
                });
            });
    }
}

/// Filtered by the search text and ordered by most recent activity first.
fn visible_chats<'a>(state: &'a AppState, search: &str) -> Vec<ChatTitle<'a>> {
    let needle = search.trim().to_lowercase();
    let mut chats: Vec<ChatTitle> = state
        .chat_titles()
        .filter(|chat| needle.is_empty() || chat.name.to_lowercase().contains(&needle))
        .collect();
    chats.sort_by_key(|chat| std::cmp::Reverse(last_activity_ms(state, chat)));
    chats
}

fn last_activity_ms(state: &AppState, chat: &ChatTitle) -> u64 {
    state
        .previews
        .get(&chat.conversation)
        .map_or(chat.since_ms, |message| message.sent_at_ms)
}

/// Paints one chat-list row; returns whether it was clicked.
fn chat_row(ui: &mut egui::Ui, state: &AppState, chat: &ChatTitle) -> bool {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), theme::CONTACT_ROW_HEIGHT),
        Sense::click(),
    );
    let selected = state.selected == Some(chat.conversation);
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

    let avatar_center = egui::pos2(
        rect.left() + ROW_PADDING + theme::CONTACT_AVATAR_SIZE / 2.0,
        rect.center().y,
    );
    avatar::paint(
        ui,
        avatar_center,
        theme::CONTACT_AVATAR_SIZE,
        &chat.key,
        chat.name,
        chat_photo(state, chat),
    );

    let secondary = if selected {
        theme::MUTED_TEXT_ON_SELECTED
    } else {
        theme::MUTED_TEXT
    };
    let text_left = avatar_center.x + theme::CONTACT_AVATAR_SIZE / 2.0 + ROW_TEXT_GAP;
    let text_right = rect.right() - ROW_PADDING;
    let top_line_y = rect.top() + rect.height() * 0.3;
    let bottom_line_y = rect.top() + rect.height() * 0.7;

    let last = state.previews.get(&chat.conversation);
    let stamp_width = match last {
        Some(message) => paint_right(
            ui,
            text_right,
            top_line_y,
            &time_fmt::list_stamp(message.sent_at_ms),
            secondary,
        ),
        None => 0.0,
    };
    let name_right = text_right - stamp_width - ROW_TEXT_GAP;
    paint_clipped(
        ui,
        text_left,
        name_right,
        top_line_y,
        chat.name,
        FontId::proportional(NAME_FONT_SIZE),
        theme::TEXT,
    );

    let unread = state.unread.get(&chat.conversation).copied().unwrap_or(0);
    let badge_width = if unread > 0 {
        paint_badge(ui, text_right, bottom_line_y, unread)
    } else {
        0.0
    };
    let preview_right =
        text_right - badge_width - if badge_width > 0.0 { ROW_TEXT_GAP } else { 0.0 };
    let preview = preview_text(last);
    paint_clipped(
        ui,
        text_left,
        preview_right,
        bottom_line_y,
        &preview,
        FontId::proportional(PREVIEW_FONT_SIZE),
        secondary,
    );

    response.clicked()
}

/// Notes show an initial, not our own profile photo.
fn chat_photo<'a>(state: &'a AppState, chat: &ChatTitle) -> Option<&'a crate::state::AvatarImage> {
    if state.is_notes(&chat.conversation) {
        None
    } else {
        state.avatars.get(&chat.key)
    }
}

fn preview_text(last: Option<&MessageView>) -> String {
    let Some(message) = last else {
        return String::new();
    };
    let first_line = message.text.lines().next().unwrap_or_default();
    if message.system {
        return first_line.to_owned();
    }
    match (message.direction, &message.sender) {
        (Direction::Outgoing, _) => format!("{OWN_MESSAGE_PREFIX}{first_line}"),
        (Direction::Incoming, Some(sender)) => format!("{}: {first_line}", sender.display_name),
        (Direction::Incoming, None) => first_line.to_owned(),
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
fn paint_clipped(
    ui: &egui::Ui,
    left: f32,
    right: f32,
    center_y: f32,
    text: &str,
    font: FontId,
    color: egui::Color32,
) {
    if right <= left {
        return;
    }
    let clip = Rect::from_min_max(
        egui::pos2(left, center_y - theme::CONTACT_ROW_HEIGHT / 2.0),
        egui::pos2(right, center_y + theme::CONTACT_ROW_HEIGHT / 2.0),
    );
    ui.painter().with_clip_rect(clip).text(
        egui::pos2(left, center_y),
        Align2::LEFT_CENTER,
        text,
        font,
        color,
    );
}

/// Paints the unread pill at the row's right edge and returns its width.
fn paint_badge(ui: &egui::Ui, right: f32, center_y: f32, unread: usize) -> f32 {
    let label = if unread > MAX_BADGE_COUNT {
        format!("{MAX_BADGE_COUNT}+")
    } else {
        unread.to_string()
    };
    let font = FontId::proportional(TIME_FONT_SIZE);
    let galley = ui
        .painter()
        .layout_no_wrap(label, font, egui::Color32::WHITE);
    let width = (galley.size().x + BADGE_PADDING).max(BADGE_MIN_WIDTH);
    let rect = Rect::from_center_size(
        egui::pos2(right - width / 2.0, center_y),
        egui::vec2(width, BADGE_HEIGHT),
    );
    ui.painter()
        .rect_filled(rect, BADGE_HEIGHT / 2.0, theme::ACCENT);
    ui.painter().galley(
        rect.center() - galley.size() / 2.0,
        galley,
        egui::Color32::WHITE,
    );
    width
}

#[cfg(test)]
mod tests {
    use brumm_core::api::ContactView;

    use super::*;

    fn invitation(stage: InvitationStage) -> GroupInvitationView {
        GroupInvitationView {
            group: [1; 32],
            name: "Chess club".into(),
            inviter: [2; 32],
            inviter_name: "Alice".into(),
            member_count: 3,
            stage,
        }
    }

    #[test]
    fn an_answered_invitation_says_what_it_waits_for() {
        assert_eq!(invitation_status(&invitation(InvitationStage::Open)), None);
        assert_eq!(
            invitation_status(&invitation(InvitationStage::Accepted)).as_deref(),
            Some("Waiting for Alice to add you…")
        );
        assert!(invitation_status(&invitation(InvitationStage::Loading)).is_some());
    }

    fn frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        state: &mut AppState,
        view: &mut ViewState,
    ) -> f32 {
        let screen_rect = Some(Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(960.0, 680.0),
        ));
        let input = egui::RawInput {
            screen_rect,
            events,
            ..Default::default()
        };
        let mut width = 0.0;
        let mut output = ctx.run_ui(input, |ui| {
            let mut commands = Vec::new();
            show(ui, state, view, &mut commands, false);
            width = ui.available_rect_before_wrap().left();
        });
        output.textures_delta.clear();
        width
    }

    #[test]
    fn search_pill_sits_between_the_buttons() {
        let ctx = egui::Context::default();
        crate::ui::theme::apply(&ctx);
        let (mut state, mut view) = (AppState::default(), ViewState::default());
        let width = frame(&ctx, vec![], &mut state, &mut view);
        let pill = SEARCH_RECT.with(|cell| cell.get()).expect("search field");
        let menu = MENU_RECT.with(|cell| cell.get()).expect("menu");
        let end = END_RECT.with(|cell| cell.get()).expect("add");
        assert!(
            menu.right() <= pill.left() && pill.right() <= end.left(),
            "pill overlaps a button: menu {menu:?} pill {pill:?} end {end:?}"
        );
        assert!(pill.width() > width - 90.0, "pill {pill:?} sidebar {width}");
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
        widths.push(frame(
            &ctx,
            vec![egui::Event::PointerMoved(edge)],
            &mut state,
            &mut view,
        ));
        widths.push(frame(&ctx, vec![button(true)], &mut state, &mut view));
        widths.push(frame(
            &ctx,
            vec![egui::Event::PointerMoved(target)],
            &mut state,
            &mut view,
        ));
        widths.push(frame(
            &ctx,
            vec![egui::Event::PointerMoved(target)],
            &mut state,
            &mut view,
        ));
        widths.push(frame(&ctx, vec![button(false)], &mut state, &mut view));
        for _ in 0..3 {
            widths.push(frame(&ctx, vec![], &mut state, &mut view));
        }
        let after_release = &widths[widths.len() - 3..];
        assert!(
            after_release
                .iter()
                .all(|width| (*width - (widths[1] + 100.0)).abs() < 1.0),
            "{widths:?}"
        );
    }

    #[test]
    fn room_search_fills_the_sidebar_from_the_open_chat() {
        use brumm_core::api::{DeliveryStatus, Trust};

        let ctx = egui::Context::default();
        crate::ui::theme::apply(&ctx);
        let contact_key = [1; 32];
        let contact = ConversationId::Direct(contact_key);
        let other = [2; 32];
        let mut state = AppState {
            ready: true,
            selected: Some(contact),
            ..Default::default()
        };
        state.contacts.push(ContactView {
            user_key: contact_key,
            display_name: "Bob".into(),
            fingerprint: "ab".into(),
            trust: Trust::Verified,
            added_at_ms: 0,
        });
        state.contacts.push(ContactView {
            user_key: other,
            display_name: "Cara".into(),
            fingerprint: "cd".into(),
            trust: Trust::Verified,
            added_at_ms: 0,
        });
        state.conversations.insert(
            contact,
            vec![MessageView {
                id: [9; 16],
                conversation: contact,
                direction: Direction::Incoming,
                text: "hello from the room".into(),
                attachment: None,
                reactions: Default::default(),
                sent_at_ms: 1_700_000_000_000,
                status: DeliveryStatus::Delivered,
                sender: None,
                system: false,
            }],
        );
        let mut view = ViewState {
            search: "cara".into(),
            ..Default::default()
        };
        view.chat.search.open(contact);
        view.chat.search.query_mut().push_str("HELLO");
        let screen_rect = Some(Rect::from_min_size(
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
                show(ui, &mut state, &mut view, &mut commands, false);
                assert!(
                    commands.is_empty(),
                    "a room search must not open another chat"
                );
            },
        );
        output.textures_delta.clear();
        assert_eq!(view.search, "cara");
        assert_eq!(view.chat.search.stored_hits(), 1);
        assert_eq!(state.selected, Some(contact));
    }
}
