//! "Group info" window for the open group: members (searched and paged by the core, so a group
//! of thousands never loads at once), roles, settings, invitations and leaving.

use std::collections::HashSet;

use brumm_core::{
    api::{Command, GroupId, GroupStatus, GroupView, MemberView, Role},
    identity::UserKey,
};

use crate::{
    state::AppState,
    ui::{
        avatar,
        files::{AvatarTarget, FileDialogs},
        new_group, theme,
    },
};

const WINDOW_WIDTH: f32 = 380.0;
const MEMBER_PAGE: u32 = 50;
const MEMBER_LIST_HEIGHT: f32 = 260.0;
/// The next page is requested once the view is this many rows from the end of the loaded ones.
const MEMBER_PREFETCH_ROWS: usize = 10;

#[derive(Default)]
pub struct GroupInfoView {
    open: Option<GroupId>,
    /// Query and member count of the last `LoadMembers` request.
    requested: Option<(String, u32)>,
    query: String,
    /// Loaded member count the last "next page" request was sent at (asks once per page).
    paged_to: Option<usize>,
    rename: String,
    inviting: bool,
    invite_filter: String,
    picked: HashSet<UserKey>,
    confirm_leave: bool,
    /// Where the window was drawn last frame.
    #[cfg(test)]
    rect: Option<egui::Rect>,
}

impl GroupInfoView {
    pub fn open(&mut self, group: &GroupView) {
        *self = Self {
            open: Some(group.id),
            rename: group.name.clone(),
            ..Self::default()
        };
    }
}

pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    view: &mut GroupInfoView,
    dialogs: &FileDialogs,
    commands: &mut Vec<Command>,
) {
    let Some(group) = view.open.and_then(|id| state.groups.get(&id)) else {
        view.open = None;
        return;
    };
    let mut open = true;
    let _shown = theme::dialog("Group info", ui.ctx(), WINDOW_WIDTH)
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            summary(ui, state, group, view, dialogs, commands);
            if group.status != GroupStatus::Active {
                return;
            }
            ui.separator();
            if group.is_admin() {
                settings(ui, group, commands);
                ui.separator();
                invite_section(ui, state, group, view, commands);
                ui.separator();
            }
            members(ui, state, group, view, commands);
            ui.separator();
            leave_section(ui, group, view, commands);
        });
    #[cfg(test)]
    {
        view.rect = _shown.map(|shown| shown.response.rect);
    }
    if !open {
        view.open = None;
    }
}

fn summary(
    ui: &mut egui::Ui,
    state: &AppState,
    group: &GroupView,
    view: &mut GroupInfoView,
    dialogs: &FileDialogs,
    commands: &mut Vec<Command>,
) {
    let photo = state.avatars.get(&group.id);
    ui.horizontal(|ui| {
        avatar::show(ui, theme::HEADER_AVATAR_SIZE, &group.id, &group.name, photo);
        if !group.is_admin() || group.status != GroupStatus::Active {
            return;
        }
        if ui.button("Set group photo…").clicked() {
            dialogs.pick_avatar(ui.ctx(), AvatarTarget::Group(group.id));
        }
        if photo.is_some() && ui.button("Remove").clicked() {
            commands.push(AvatarTarget::Group(group.id).command(Vec::new()));
        }
    });
    if group.is_admin() && group.status == GroupStatus::Active {
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut view.rename).desired_width(220.0));
            let name = view.rename.trim();
            if ui
                .add_enabled(
                    !name.is_empty() && name != group.name,
                    egui::Button::new("Rename"),
                )
                .clicked()
            {
                commands.push(Command::RenameGroup {
                    group: group.id,
                    name: name.to_owned(),
                });
            }
        });
    } else {
        ui.heading(&group.name);
    }
    let status = match group.status {
        GroupStatus::Active => format!("{} members", group.member_count),
        GroupStatus::Left => "You left this group".to_owned(),
        GroupStatus::Removed => "You were removed from this group".to_owned(),
    };
    ui.label(egui::RichText::new(status).color(theme::MUTED_TEXT));
}

fn settings(ui: &mut egui::Ui, group: &GroupView, commands: &mut Vec<Command>) {
    let mut announcement = group.announcement;
    if ui
        .checkbox(&mut announcement, "Only admins can post")
        .changed()
    {
        commands.push(Command::SetAnnouncement {
            group: group.id,
            on: announcement,
        });
    }
    let mut sharing = group.history_sharing;
    if ui
        .checkbox(&mut sharing, "Share the last week with new members")
        .changed()
    {
        commands.push(Command::SetHistorySharing {
            group: group.id,
            on: sharing,
        });
    }
    if ui
        .button("Refresh group key")
        .on_hover_text("Replaces the key everyone encrypts with, e.g. after a lost device")
        .clicked()
    {
        commands.push(Command::RefreshGroupKeys { group: group.id });
    }
}

fn invite_section(
    ui: &mut egui::Ui,
    state: &AppState,
    group: &GroupView,
    view: &mut GroupInfoView,
    commands: &mut Vec<Command>,
) {
    if !view.inviting {
        if ui.button("Invite contacts…").clicked() {
            view.inviting = true;
        }
        return;
    }
    ui.add(
        egui::TextEdit::singleline(&mut view.invite_filter)
            .hint_text("Search contacts")
            .desired_width(f32::INFINITY),
    );
    new_group::contact_picker(ui, state, &view.invite_filter, &mut view.picked);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!view.picked.is_empty(), egui::Button::new("Send invitations"))
            .clicked()
        {
            commands.push(Command::InviteToGroup {
                group: group.id,
                users: view.picked.drain().collect(),
            });
            view.inviting = false;
        }
        if ui.button("Cancel").clicked() {
            view.inviting = false;
            view.picked.clear();
        }
    });
}

fn members(
    ui: &mut egui::Ui,
    state: &AppState,
    group: &GroupView,
    view: &mut GroupInfoView,
    commands: &mut Vec<Command>,
) {
    ui.add(
        egui::TextEdit::singleline(&mut view.query)
            .hint_text("Search members")
            .desired_width(f32::INFINITY),
    );
    let query = view.query.trim().to_owned();
    // Asks again when the query or the member count changes (someone joined or left).
    let wanted = (query.clone(), group.member_count);
    if view.requested.as_ref() != Some(&wanted) {
        view.requested = Some(wanted);
        view.paged_to = None;
        commands.push(Command::LoadMembers {
            group: group.id,
            query: query.clone(),
            offset: 0,
            limit: MEMBER_PAGE,
        });
    }
    let page = state
        .members
        .as_ref()
        .filter(|page| page.group == group.id && page.query == query);
    let Some(page) = page else {
        ui.spinner();
        return;
    };
    let me = state.profile.as_ref().map(|profile| profile.user_key);
    // Only the rows on screen are laid out; the next page is asked for near the end.
    let loaded = page.members.len();
    let row_height = ui.spacing().interact_size.y;
    egui::ScrollArea::vertical()
        .max_height(MEMBER_LIST_HEIGHT)
        .auto_shrink([false, true])
        .show_rows(ui, row_height, loaded, |ui, rows| {
            let near_end = rows.end + MEMBER_PREFETCH_ROWS >= loaded;
            for member in &page.members[rows] {
                member_row(ui, group, member, me == Some(member.user_key), commands);
            }
            let more = (loaded as u32) < page.total;
            if near_end && more && view.paged_to != Some(loaded) {
                view.paged_to = Some(loaded);
                commands.push(Command::LoadMembers {
                    group: group.id,
                    query: query.clone(),
                    offset: loaded as u32,
                    limit: MEMBER_PAGE,
                });
            }
        });
}

fn member_row(
    ui: &mut egui::Ui,
    group: &GroupView,
    member: &MemberView,
    is_me: bool,
    commands: &mut Vec<Command>,
) {
    ui.horizontal(|ui| {
        let name = if is_me {
            format!("{} (you)", member.display_name)
        } else {
            member.display_name.clone()
        };
        ui.label(name);
        let role = match member.role {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "",
        };
        if !role.is_empty() {
            ui.label(egui::RichText::new(role).small().color(theme::ACCENT_LIGHT));
        }
        let manageable = !is_me && group.is_admin() && member.role != Role::Owner;
        if !manageable {
            return;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("⋯", |ui| {
                let user = member.user_key;
                let promote = member.role == Role::Member;
                let label = if promote { "Make admin" } else { "Remove admin" };
                if group.my_role == Some(Role::Owner) && ui.button(label).clicked() {
                    commands.push(Command::SetRole {
                        group: group.id,
                        user,
                        admin: promote,
                    });
                    ui.close();
                }
                if ui
                    .button(egui::RichText::new("Remove from group").color(theme::DANGER))
                    .clicked()
                {
                    commands.push(Command::RemoveMember {
                        group: group.id,
                        user,
                    });
                    ui.close();
                }
            });
        });
    });
}

fn leave_section(
    ui: &mut egui::Ui,
    group: &GroupView,
    view: &mut GroupInfoView,
    commands: &mut Vec<Command>,
) {
    if !view.confirm_leave {
        if ui
            .button(egui::RichText::new("Leave group").color(theme::DANGER))
            .clicked()
        {
            view.confirm_leave = true;
        }
        return;
    }
    let hint = if group.my_role == Some(Role::Owner) {
        "The first admin (or the longest member) becomes the owner."
    } else {
        "You will no longer receive messages from this group."
    };
    ui.label(egui::RichText::new(hint).color(theme::MUTED_TEXT));
    ui.horizontal(|ui| {
        if ui.add(theme::danger_button("Leave")).clicked() {
            commands.push(Command::LeaveGroup { group: group.id });
            view.confirm_leave = false;
        }
        if ui.button("Cancel").clicked() {
            view.confirm_leave = false;
        }
    });
}

#[cfg(test)]
mod tests {
    use brumm_core::api::GroupStatus;

    use super::*;
    use crate::state::MembersPage;

    const PHONE: egui::Vec2 = egui::vec2(360.0, 720.0);
    const MEMBERS: u32 = 2000;
    const LOADED: usize = 100;

    #[test]
    fn a_big_group_fits_a_phone_and_pages_lazily() {
        let ctx = egui::Context::default();
        crate::ui::theme::apply(&ctx);
        let group = GroupView {
            id: [5; 32],
            name: "Crowd".into(),
            status: GroupStatus::Active,
            my_role: Some(Role::Owner),
            announcement: false,
            history_sharing: false,
            member_count: MEMBERS,
            owner: Some([0; 32]),
        };
        let mut state = AppState::default();
        state.groups.insert(group.id, group.clone());
        state.members = Some(MembersPage {
            group: group.id,
            query: String::new(),
            members: (0..LOADED)
                .map(|index| MemberView {
                    user_key: [index as u8; 32],
                    display_name: format!("Member {index}"),
                    role: Role::Member,
                })
                .collect(),
            total: MEMBERS,
        });
        let mut view = GroupInfoView::default();
        view.open(&group);
        let dialogs = FileDialogs::default();
        let mut commands = Vec::new();
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, PHONE)),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| show(ui, &state, &mut view, &dialogs, &mut commands));
            output.textures_delta.clear();
        }
        let offsets: Vec<u32> = commands
            .iter()
            .filter_map(|command| match command {
                Command::LoadMembers { offset, .. } => Some(*offset),
                _ => None,
            })
            .collect();
        assert_eq!(offsets, vec![0], "only the first page while the list is at its top");
        let window = view.rect.expect("window shown");
        assert!(window.left() >= 0.0 && window.right() <= PHONE.x, "{window:?} off screen");
    }
}
