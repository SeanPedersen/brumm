//! Renders text as a QR code texture (cached per text so it is encoded only once).

use qrcode::{Color, QrCode};

const QUIET_ZONE_MODULES: usize = 4;
const DARK: u8 = 0;
const LIGHT: u8 = 255;

#[derive(Default)]
pub struct QrCache {
    cached: Option<(String, egui::TextureHandle)>,
}

impl QrCache {
    pub fn texture(&mut self, ctx: &egui::Context, text: &str) -> Option<&egui::TextureHandle> {
        let stale = self.cached.as_ref().is_none_or(|(cached_text, _)| cached_text != text);
        if stale {
            let image = encode(text)?;
            let texture = ctx.load_texture("invite_qr", image, egui::TextureOptions::NEAREST);
            self.cached = Some((text.to_owned(), texture));
        }
        self.cached.as_ref().map(|(_, texture)| texture)
    }
}

fn encode(text: &str) -> Option<egui::ColorImage> {
    let code = QrCode::new(text.as_bytes()).ok()?;
    let modules = code.width();
    let colors = code.to_colors();
    let side = modules + 2 * QUIET_ZONE_MODULES;
    let mut gray = vec![LIGHT; side * side];
    for (index, color) in colors.iter().enumerate() {
        if *color == Color::Dark {
            let (row, col) = (index / modules, index % modules);
            gray[(row + QUIET_ZONE_MODULES) * side + col + QUIET_ZONE_MODULES] = DARK;
        }
    }
    Some(egui::ColorImage::from_gray([side, side], &gray))
}
