//! Markdown in text messages (CommonMark + GFM tables, strikethrough and task lists), always on.
//! `parse` is pure, `layout` turns the result into galleys and decorations for the bubble, and
//! `fonts` provides the bold face. A message that reads the same as plain text returns `None`
//! and keeps the plain path.

mod fonts;
mod layout;
mod parse;

use std::sync::Arc;

use egui::cache::{ComputerMut, FrameCache};

pub use fonts::{install as install_fonts, is_bold_family};
pub use layout::{Rendered, layout};
pub use parse::Doc;

#[derive(Default)]
struct DocParser;

impl ComputerMut<&str, Option<Arc<Doc>>> for DocParser {
    fn compute(&mut self, text: &str) -> Option<Arc<Doc>> {
        let doc = parse::parse(text);
        (!doc.is_plain(text)).then(|| Arc::new(doc))
    }
}

type DocCache = FrameCache<Option<Arc<Doc>>, DocParser>;

/// The parsed message, or `None` when it has no markdown. Cached while the message stays on screen.
pub fn doc(ctx: &egui::Context, text: &str) -> Option<Arc<Doc>> {
    ctx.memory_mut(|memory| memory.caches.cache::<DocCache>().get(text).clone())
}
