//! "Syncing history" hint in a chat whose initial history is still arriving on this freshly
//! linked device: centered in an empty chat, a small pill on top once messages are shown
//! (the newest arrive first, so what is missing is the older part).

use egui::{Align2, Rect, vec2};

use crate::ui::theme;

/// Distance of the pill from the top of the message list.
const PILL_TOP_MARGIN: f32 = 10.0;
const PILL_PADDING: egui::Vec2 = vec2(12.0, 6.0);
const PILL_RADIUS: u8 = 14;
const EMPTY_HINT_WIDTH: f32 = 320.0;
/// The empty-chat hint starts at this fraction of the list height.
const EMPTY_HINT_TOP_FRACTION: f32 = 0.35;
const PILL_FONT_SIZE: f32 = 13.0;
const SPINNER_GAP: f32 = 8.0;

pub struct SyncProgress {
    pub received: u32,
    pub expected: u32,
}

impl SyncProgress {
    fn count(&self) -> String {
        format!("{} of {} messages", self.received, self.expected)
    }
}

/// `area` is the message list's visible rect.
pub fn show(ui: &mut egui::Ui, area: Rect, progress: &SyncProgress, chat_is_empty: bool) {
    if chat_is_empty {
        empty_chat(ui, area, progress);
    } else {
        top_pill(ui, area, progress);
    }
}

fn empty_chat(ui: &mut egui::Ui, area: Rect, progress: &SyncProgress) {
    let rect = Rect::from_center_size(area.center(), vec2(EMPTY_HINT_WIDTH, area.height()));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Center)),
        |ui| {
            ui.add_space(area.height() * EMPTY_HINT_TOP_FRACTION);
            ui.spinner();
            ui.add_space(SPINNER_GAP);
            ui.label(egui::RichText::new("Syncing history from your other devices…").strong());
            ui.label(egui::RichText::new(progress.count()).color(theme::MUTED_TEXT));
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Keep one of them online until it is done.")
                    .small()
                    .color(theme::MUTED_TEXT),
            );
        },
    );
}

fn top_pill(ui: &mut egui::Ui, area: Rect, progress: &SyncProgress) {
    let text = format!("Syncing older messages… {}", progress.count());
    let galley = ui.painter().layout_no_wrap(
        text,
        egui::FontId::proportional(PILL_FONT_SIZE),
        theme::TEXT,
    );
    let spinner_size = galley.size().y;
    let size = vec2(
        spinner_size + SPINNER_GAP + galley.size().x,
        galley.size().y,
    ) + PILL_PADDING * 2.0;
    let rect =
        Align2::CENTER_TOP.align_size_within_rect(size, area.shrink2(vec2(0.0, PILL_TOP_MARGIN)));
    ui.painter()
        .rect_filled(rect, PILL_RADIUS, theme::SERVICE_PILL);
    let spinner_rect =
        Rect::from_min_size(rect.min + PILL_PADDING, vec2(spinner_size, spinner_size));
    egui::Spinner::new()
        .size(spinner_size)
        .color(theme::MUTED_TEXT)
        .paint_at(ui, spinner_rect);
    ui.painter().galley(
        rect.min + PILL_PADDING + vec2(spinner_size + SPINNER_GAP, 0.0),
        galley,
        theme::TEXT,
    );
}
