//! System color emoji font, installed after the text fonts.
//!
//! The system font fallback tries a sans-serif before the emoji family. DejaVu
//! Sans and Noto Sans Symbols 2 have monochrome outlines for some emoji, so
//! those draw as grey silhouettes while the rest come from the color font.
//! Installing the color font up front, still behind the text fonts, makes every
//! emoji it covers render in color. Letters and digits stay with the text font.

use std::path::PathBuf;
use std::sync::OnceLock;

use egui::epaint::text::{
    FontData, FontFamily, FontInsert, FontPriority, FontProvider, InsertFontFamily,
};

const FONT_NAME: &str = "color-emoji";

/// Distro paths checked before asking fontconfig. `fc-match` covers the rest.
const WELL_KNOWN_PATHS: &[&str] = &[
    "/usr/share/fonts/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
    "/usr/share/fonts/google-noto-emoji/NotoColorEmoji.ttf",
    "/usr/share/fonts/truetype/NotoColorEmoji.ttf",
];

/// Bitmap (`CBDT`, `sbix`) or `COLR` paint tables. An outline-only font is not useful here.
const COLOR_TABLE_TAGS: [&[u8; 4]; 3] = [b"CBDT", b"COLR", b"sbix"];

/// TrueType, OpenType (`OTTO`), and the legacy `true` magic.
const SFNT_MAGICS: [u32; 3] = [
    0x0001_0000,
    u32::from_be_bytes(*b"OTTO"),
    u32::from_be_bytes(*b"true"),
];

const SFNT_HEADER_LEN: usize = 12;
const TABLE_RECORD_LEN: usize = 16;

/// Loads the system color emoji font once and offers it to every text family.
pub struct ColorEmojiFont {
    data: OnceLock<Option<FontData>>,
}

impl ColorEmojiFont {
    pub fn new() -> Self {
        Self {
            data: OnceLock::new(),
        }
    }

    fn data(&self) -> Option<FontData> {
        self.data.get_or_init(load_color_emoji).clone()
    }
}

impl Default for ColorEmojiFont {
    fn default() -> Self {
        Self::new()
    }
}

impl FontProvider for ColorEmojiFont {
    fn fonts_for_family(&self, family: &FontFamily) -> Vec<FontInsert> {
        if !matches!(family, FontFamily::Proportional | FontFamily::Monospace) {
            return Vec::new();
        }
        let Some(data) = self.data() else {
            return Vec::new();
        };
        vec![FontInsert::new(
            FONT_NAME,
            data,
            vec![InsertFontFamily {
                family: family.clone(),
                priority: FontPriority::Lowest,
            }],
        )]
    }
}

fn load_color_emoji() -> Option<FontData> {
    let path = emoji_font_path()?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(path = %path.display(), "cannot read color emoji font: {err}");
            return None;
        }
    };
    if !has_color_table(&bytes) {
        tracing::warn!(path = %path.display(), "emoji font has no color glyphs");
        return None;
    }
    tracing::debug!(path = %path.display(), "using color emoji font");
    Some(FontData::from_owned(bytes))
}

fn emoji_font_path() -> Option<PathBuf> {
    WELL_KNOWN_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .or_else(fc_match_emoji)
}

fn fc_match_emoji() -> Option<PathBuf> {
    let output = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "emoji"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(path.trim());
    path.is_file().then_some(path)
}

/// True when the sfnt directory lists a color-glyph table.
fn has_color_table(bytes: &[u8]) -> bool {
    let Some(header) = bytes.get(..SFNT_HEADER_LEN) else {
        return false;
    };
    let Ok(magic_bytes) = <[u8; 4]>::try_from(&header[..4]) else {
        return false;
    };
    if !SFNT_MAGICS.contains(&u32::from_be_bytes(magic_bytes)) {
        return false;
    }
    let Ok(count_bytes) = <[u8; 2]>::try_from(&header[4..6]) else {
        return false;
    };
    let table_count = usize::from(u16::from_be_bytes(count_bytes));
    let directory_end = SFNT_HEADER_LEN + table_count * TABLE_RECORD_LEN;
    let Some(directory) = bytes.get(SFNT_HEADER_LEN..directory_end) else {
        return false;
    };
    directory
        .as_chunks::<TABLE_RECORD_LEN>()
        .0
        .iter()
        .any(|record| {
            COLOR_TABLE_TAGS
                .iter()
                .any(|tag| record.starts_with(tag.as_slice()))
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use egui::epaint::text::{
        FallbackRequest, FontData, FontDefinitions, FontInsert, FontPriority, FontProvider, Fonts,
        InsertFontFamily, TextOptions,
    };
    use egui::{Color32, FontId};

    use super::*;

    const DEJAVU_SANS: &str = "/usr/share/fonts/TTF/DejaVuSans.ttf";

    /// Emoji whose outlines also live in DejaVu Sans, so a sans fallback paints them grey.
    const MONOCHROME_STEAL: &str = "😀❤☁⚡✌✈☕";

    /// Emoji the color font has and DejaVu Sans does not.
    const COLOR_ONLY: &str = "🙂🤓👍🔒";

    /// Returns DejaVu Sans for every missing character, the way a sans-serif fallback does.
    struct MonochromeSteal {
        data: FontData,
    }

    impl FontProvider for MonochromeSteal {
        fn font_for(&self, request: &FallbackRequest<'_>) -> Option<FontInsert> {
            Some(FontInsert::new(
                "dejavu-sans",
                self.data.clone(),
                vec![InsertFontFamily {
                    family: request.family.clone(),
                    priority: FontPriority::Lowest,
                }],
            ))
        }
    }

    fn sfnt_with_tables(tags: &[&[u8; 4]]) -> Vec<u8> {
        let mut bytes = vec![0, 1, 0, 0];
        bytes.extend(u16::try_from(tags.len()).unwrap().to_be_bytes());
        bytes.extend([0, 0, 0, 0, 0, 0]);
        for tag in tags {
            bytes.extend(tag.as_slice());
            bytes.extend([0; 12]);
        }
        bytes
    }

    fn deja_vu() -> FontData {
        let bytes = std::fs::read(DEJAVU_SANS).expect("DejaVu Sans");
        FontData::from_owned(bytes)
    }

    fn layout_flags(providers: Vec<Arc<dyn FontProvider>>, text: &str) -> Vec<(char, bool)> {
        let mut fonts = Fonts::new(TextOptions::default(), FontDefinitions::default())
            .with_font_providers(providers);
        let galley = fonts.with_pixels_per_point(1.0).layout_no_wrap(
            text.to_owned(),
            FontId::proportional(22.0),
            Color32::WHITE,
        );
        galley.rows[0]
            .glyphs
            .iter()
            .map(|glyph| (glyph.chr, glyph.is_color))
            .collect()
    }

    #[test]
    fn color_table_scan_reads_the_sfnt_directory() {
        assert!(has_color_table(&sfnt_with_tables(&[b"cmap", b"CBDT"])));
        assert!(has_color_table(&sfnt_with_tables(&[b"COLR"])));
        assert!(has_color_table(&sfnt_with_tables(&[b"sbix"])));
        assert!(!has_color_table(&sfnt_with_tables(&[b"cmap", b"glyf"])));
        assert!(!has_color_table(&[]));
        let path = emoji_font_path().expect("color emoji font");
        let bytes = std::fs::read(&path).unwrap();
        assert!(has_color_table(&bytes), "{}", path.display());
    }

    #[test]
    fn sans_fallback_paints_shared_emoji_as_outlines() {
        let flags = layout_flags(
            vec![Arc::new(MonochromeSteal { data: deja_vu() })],
            MONOCHROME_STEAL,
        );
        assert!(flags.iter().all(|(_, color)| !color), "{flags:?}");
    }

    #[test]
    fn color_font_wins_over_the_sans_fallback() {
        let providers: Vec<Arc<dyn FontProvider>> = vec![
            Arc::new(MonochromeSteal { data: deja_vu() }),
            Arc::new(ColorEmojiFont::new()),
        ];
        let sample = format!("{MONOCHROME_STEAL}{COLOR_ONLY}A0#");
        let flags = layout_flags(providers, &sample);
        let colored: String = flags
            .iter()
            .filter_map(|(chr, color)| color.then_some(*chr))
            .collect();
        let expected: String = format!("{MONOCHROME_STEAL}{COLOR_ONLY}").chars().collect();
        assert_eq!(colored, expected, "{flags:?}");
        for chr in ['A', '0', '#'] {
            let flag = flags.iter().find(|(glyph, _)| *glyph == chr);
            assert_eq!(flag, Some(&(chr, false)), "{flags:?}");
        }
    }
}
