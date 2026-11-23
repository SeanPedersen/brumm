//! Round avatar with the contact's initials on a color derived from their key.

use brumm_core::identity::UserKey;
use egui::{Align2, FontId, Pos2, Ui};

use crate::{state::AvatarImage, ui::theme};

const INITIAL_COUNT: usize = 2;
const INITIAL_FONT_FRACTION: f32 = 0.38;

/// Paints an avatar centered on `center`: the photo if there is one, else initials on a color.
pub fn paint(
    ui: &Ui,
    center: Pos2,
    diameter: f32,
    key: &UserKey,
    name: &str,
    photo: Option<&AvatarImage>,
) {
    if let Some(photo) = photo {
        let rect = egui::Rect::from_center_size(center, egui::vec2(diameter, diameter));
        let image = egui::Image::from_bytes(
            photo.uri.clone(),
            egui::load::Bytes::Shared(photo.bytes.clone()),
        )
        .corner_radius(egui::CornerRadius::same(u8::MAX));
        image.paint_at(ui, rect);
        return;
    }
    ui.painter()
        .circle_filled(center, diameter / 2.0, color_of(key));
    ui.painter().text(
        center,
        Align2::CENTER_CENTER,
        initials(name),
        FontId::proportional(diameter * INITIAL_FONT_FRACTION),
        egui::Color32::WHITE,
    );
}

/// The color of a key's initials circle (also used for its name in group chats).
pub fn color_of(key: &UserKey) -> egui::Color32 {
    let index = key.iter().map(|byte| *byte as usize).sum::<usize>() % theme::AVATAR_COLORS.len();
    theme::AVATAR_COLORS[index]
}

/// Allocates space for an avatar in the current layout and paints it.
pub fn show(ui: &mut Ui, diameter: f32, key: &UserKey, name: &str, photo: Option<&AvatarImage>) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(diameter, diameter), egui::Sense::hover());
    paint(ui, rect.center(), diameter, key, name, photo);
}

fn initials(name: &str) -> String {
    let letters: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(INITIAL_COUNT)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() {
        "?".to_owned()
    } else {
        letters
    }
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn takes_first_letters_of_first_two_words() {
        assert_eq!(initials("alice bob carol"), "AB");
        assert_eq!(initials("zoe"), "Z");
        assert_eq!(initials("  "), "?");
    }
}
