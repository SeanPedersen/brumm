//! The bold family for markdown. egui bundles only Ubuntu Light, so strong text uses a bundled
//! Ubuntu Medium (same family and licence), followed by egui's own proportional fallback chain so
//! emoji and symbols inside bold text still draw.

use egui::epaint::text::{FontData, FontDefinitions, FontFamily, FontInsert, FontProvider};

const BOLD_FAMILY_NAME: &str = "bold";
const BOLD_FONT_NAME: &str = "Ubuntu-Medium";
static BOLD_FONT: &[u8] = include_bytes!("../../../assets/fonts/Ubuntu-Medium.ttf");

pub fn bold_family() -> FontFamily {
    FontFamily::Name(BOLD_FAMILY_NAME.into())
}

pub fn is_bold_family(family: &FontFamily) -> bool {
    matches!(family, FontFamily::Name(name) if name.as_ref() == BOLD_FAMILY_NAME)
}

/// Heads the bold family. Providers are asked in install order, so this goes in before
/// [`crate::ui::emoji_font::ColorEmojiFont`], which then appends the color emoji to it.
pub struct BoldFont;

impl FontProvider for BoldFont {
    fn fonts_for_family(&self, family: &FontFamily) -> Vec<FontInsert> {
        if !is_bold_family(family) {
            return Vec::new();
        }
        let bold = FontInsert::new(BOLD_FONT_NAME, FontData::from_static(BOLD_FONT), Vec::new());
        std::iter::once(bold)
            .chain(FontDefinitions::default().fonts_for_family(&FontFamily::Proportional))
            .collect()
    }
}

pub fn install(ctx: &egui::Context) {
    ctx.add_font_provider(std::sync::Arc::new(BoldFont));
}
