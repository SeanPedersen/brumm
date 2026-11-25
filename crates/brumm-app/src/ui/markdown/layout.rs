//! [`Doc`] → positioned galleys plus decorations (code backgrounds, quote bars, list markers,
//! table grid), all relative to the content origin. The bubble measures the result, paints it
//! and hit-tests its links like a plain text galley.

use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, CornerRadius, FontFamily, FontId, Galley, Pos2, Rect, Stroke, Vec2};

use super::fonts::bold_family;
use super::parse::{Block, BlockKind, ColumnAlign, Doc, Inline, Marker, Table};
use crate::ui::{links::TextLink, theme};

/// Indexed by heading level − 1.
const HEADING_SIZES: [f32; 6] = [21.0, 19.0, 17.0, 16.0, 15.0, 15.0];
const CODE_FONT_SCALE: f32 = 0.9;
const BLOCK_GAP: f32 = 6.0;
const LIST_ITEM_GAP: f32 = 3.0;
const QUOTE_INDENT: f32 = 12.0;
const QUOTE_BAR_WIDTH: f32 = 3.0;
const LIST_INDENT: f32 = 22.0;
const MARKER_GAP: f32 = 6.0;
const BULLET_RADIUS: f32 = 2.5;
const BULLET_STROKE: f32 = 1.2;
const CHECKBOX_EDGE: f32 = 12.0;
const CHECKBOX_RADIUS: u8 = 3;
const CODE_BLOCK_PADDING: Vec2 = egui::vec2(8.0, 6.0);
const CODE_BLOCK_RADIUS: u8 = 6;
const RULE_HEIGHT: f32 = 12.0;
const LINE_WIDTH: f32 = 1.0;
const TABLE_CELL_PADDING: Vec2 = egui::vec2(8.0, 4.0);
const TABLE_MIN_COLUMN_WIDTH: f32 = 40.0;
/// Width of an empty paragraph's placeholder, so an empty list item keeps a line of height.
const EMPTY_LINE_TEXT: &str = " ";
const CODE_BG: Color32 = Color32::from_black_alpha(70);
const TABLE_HEADER_BG: Color32 = Color32::from_rgba_premultiplied(10, 10, 10, 10);
const LINE_COLOR: Color32 = Color32::from_rgba_premultiplied(45, 45, 45, 45);

pub struct PlacedText {
    pub offset: Vec2,
    pub galley: Arc<Galley>,
    pub links: Vec<TextLink>,
}

enum Decoration {
    Fill {
        rect: Rect,
        color: Color32,
        radius: CornerRadius,
        /// Code blocks and table headers reach the content's right edge once it is known.
        stretch: bool,
    },
    Line {
        from: Pos2,
        to: Pos2,
        /// A horizontal rule runs to the content's right edge.
        stretch: bool,
    },
    Bullet {
        center: Pos2,
        filled: bool,
    },
    Checkbox {
        rect: Rect,
        checked: bool,
    },
}

/// A laid-out markdown body. Offsets are relative to the content's top-left corner.
pub struct Rendered {
    pub texts: Vec<PlacedText>,
    decorations: Vec<Decoration>,
    pub size: Vec2,
    /// Width of the last text line, where the time may share the line. Blocks that end in a
    /// box (code, table, rule) report the full width, which puts the time below them.
    pub last_row_width: f32,
}

impl Rendered {
    pub fn paint(&self, painter: &egui::Painter, origin: Pos2) {
        let shift = origin.to_vec2();
        for decoration in &self.decorations {
            paint_decoration(painter, decoration, shift);
        }
        for text in &self.texts {
            painter.galley(origin + text.offset, text.galley.clone(), theme::TEXT);
        }
    }
}

fn paint_decoration(painter: &egui::Painter, decoration: &Decoration, shift: Vec2) {
    match decoration {
        Decoration::Fill {
            rect,
            color,
            radius,
            ..
        } => {
            painter.rect_filled(rect.translate(shift), *radius, *color);
        }
        Decoration::Line { from, to, .. } => {
            painter.line_segment(
                [*from + shift, *to + shift],
                Stroke::new(LINE_WIDTH, LINE_COLOR),
            );
        }
        Decoration::Bullet { center, filled } => {
            if *filled {
                painter.circle_filled(*center + shift, BULLET_RADIUS, theme::TEXT);
            } else {
                painter.circle_stroke(
                    *center + shift,
                    BULLET_RADIUS,
                    Stroke::new(BULLET_STROKE, theme::TEXT),
                );
            }
        }
        Decoration::Checkbox { rect, checked } => {
            let rect = rect.translate(shift);
            let radius = CornerRadius::same(CHECKBOX_RADIUS);
            if *checked {
                painter.rect_filled(rect, radius, theme::ACCENT);
                let check = [
                    rect.lerp_inside(egui::vec2(0.22, 0.52)),
                    rect.lerp_inside(egui::vec2(0.42, 0.72)),
                    rect.lerp_inside(egui::vec2(0.78, 0.30)),
                ];
                painter.line(
                    check.to_vec(),
                    Stroke::new(BULLET_STROKE * 1.5, Color32::WHITE),
                );
            } else {
                painter.rect_stroke(
                    rect,
                    radius,
                    Stroke::new(BULLET_STROKE, theme::MUTED_TEXT),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
}

pub fn layout(ui: &egui::Ui, doc: &Doc, body_size: f32, max_width: f32) -> Rendered {
    let mut out = Builder {
        ui,
        body_size,
        max_width,
        texts: Vec::new(),
        decorations: Vec::new(),
        y: 0.0,
        width: 0.0,
        last_row_width: 0.0,
    };
    let mut previous: Option<&Block> = None;
    for block in &doc.blocks {
        if let Some(previous) = previous {
            out.y += block_gap(previous, block);
        }
        out.block(block, previous);
        previous = Some(block);
    }
    out.finish()
}

fn block_gap(previous: &Block, block: &Block) -> f32 {
    let inside_one_list = previous.list_depth > 0 && block.list_depth > 0;
    if inside_one_list {
        LIST_ITEM_GAP
    } else {
        BLOCK_GAP
    }
}

struct Builder<'a> {
    ui: &'a egui::Ui,
    body_size: f32,
    max_width: f32,
    texts: Vec<PlacedText>,
    decorations: Vec<Decoration>,
    y: f32,
    width: f32,
    last_row_width: f32,
}

impl Builder<'_> {
    fn block(&mut self, block: &Block, previous: Option<&Block>) {
        let quote_left = f32::from(block.quote_depth) * QUOTE_INDENT;
        let left = quote_left + f32::from(block.list_depth) * LIST_INDENT;
        let available = (self.max_width - left).max(1.0);
        let top = self.y;
        let first_line_height = match &block.kind {
            BlockKind::Paragraph(inline) => {
                self.text_block(inline, self.body_size, false, left, available)
            }
            BlockKind::Heading { level, text } => {
                let size = HEADING_SIZES
                    [usize::from(level.saturating_sub(1)).min(HEADING_SIZES.len() - 1)];
                self.text_block(text, size, true, left, available)
            }
            BlockKind::Code(code) => self.code_block(code, left, available),
            BlockKind::Rule => self.rule(left),
            BlockKind::Table(table) => self.table(table, left, available),
        };
        if let Some(marker) = block.marker {
            self.marker(marker, block.list_depth, left, top, first_line_height);
        }
        self.quote_bars(block, previous, top);
    }

    /// Returns the height of the first line, which the list marker is centred on.
    fn text_block(
        &mut self,
        inline: &Inline,
        size: f32,
        bold: bool,
        left: f32,
        available: f32,
    ) -> f32 {
        let (job, links) = inline_job(inline, size, bold, available);
        let galley = self.ui.painter().layout_job(job);
        let first_line = galley.rows.first().map_or(size, |row| row.rect().height());
        self.last_row_width = left + galley.rows.last().map_or(0.0, |row| row.rect().width());
        self.place(Vec2::new(left, self.y), galley, links);
        first_line
    }

    fn code_block(&mut self, code: &str, left: f32, available: f32) -> f32 {
        let font = FontId::monospace(self.body_size * CODE_FONT_SCALE);
        let wrap = (available - CODE_BLOCK_PADDING.x * 2.0).max(1.0);
        let text = if code.is_empty() {
            EMPTY_LINE_TEXT
        } else {
            code
        };
        let galley = self
            .ui
            .painter()
            .layout(text.to_owned(), font, theme::TEXT, wrap);
        let box_size = galley.size() + CODE_BLOCK_PADDING * 2.0;
        let rect = Rect::from_min_size(Pos2::new(left, self.y), box_size);
        self.decorations.push(Decoration::Fill {
            rect,
            color: CODE_BG,
            radius: CornerRadius::same(CODE_BLOCK_RADIUS),
            stretch: true,
        });
        let first_line = galley.rows.first().map_or(0.0, |row| row.rect().height());
        self.place(
            Vec2::new(left, self.y) + CODE_BLOCK_PADDING,
            galley,
            Vec::new(),
        );
        self.close_box(rect);
        first_line + CODE_BLOCK_PADDING.y * 2.0
    }

    fn rule(&mut self, left: f32) -> f32 {
        let middle = self.y + RULE_HEIGHT / 2.0;
        self.decorations.push(Decoration::Line {
            from: Pos2::new(left, middle),
            to: Pos2::new(left + 1.0, middle),
            stretch: true,
        });
        self.close_box(Rect::from_min_size(
            Pos2::new(left, self.y),
            Vec2::new(0.0, RULE_HEIGHT),
        ));
        RULE_HEIGHT
    }

    fn table(&mut self, table: &Table, left: f32, available: f32) -> f32 {
        let columns = table
            .rows
            .iter()
            .map(Vec::len)
            .chain([table.header.len()])
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return 0.0;
        }
        let widths = self.column_widths(table, columns, available);
        let top = self.y;
        let mut row_tops = vec![top];
        let rows =
            std::iter::once((&table.header, true)).chain(table.rows.iter().map(|row| (row, false)));
        for (cells, is_header) in rows {
            let row_top = self.y;
            let mut height: f32 = 0.0;
            let mut cell_left = left;
            for (column, width) in widths.iter().enumerate() {
                let Some(cell) = cells.get(column) else {
                    cell_left += width;
                    continue;
                };
                let wrap = (width - TABLE_CELL_PADDING.x * 2.0).max(1.0);
                let (job, links) = inline_job(cell, self.body_size, is_header, wrap);
                let galley = self.ui.painter().layout_job(job);
                let slack = width - TABLE_CELL_PADDING.x * 2.0 - galley.size().x;
                let align_shift = match table.alignments.get(column) {
                    Some(ColumnAlign::Right) => slack,
                    Some(ColumnAlign::Center) => slack / 2.0,
                    _ => 0.0,
                }
                .max(0.0);
                height = height.max(galley.size().y);
                let offset = Vec2::new(
                    cell_left + TABLE_CELL_PADDING.x + align_shift,
                    row_top + TABLE_CELL_PADDING.y,
                );
                self.place(offset, galley, links);
                cell_left += width;
            }
            self.y = row_top + height + TABLE_CELL_PADDING.y * 2.0;
            if is_header {
                self.decorations.push(Decoration::Fill {
                    rect: Rect::from_min_max(
                        Pos2::new(left, row_top),
                        Pos2::new(cell_left, self.y),
                    ),
                    color: TABLE_HEADER_BG,
                    radius: CornerRadius::ZERO,
                    stretch: false,
                });
            }
            row_tops.push(self.y);
        }
        let right = left + widths.iter().sum::<f32>();
        self.grid(left, right, &row_tops, &widths);
        let first_row = row_tops.get(1).copied().unwrap_or(top) - top;
        self.close_box(Rect::from_min_max(
            Pos2::new(left, top),
            Pos2::new(right, self.y),
        ));
        first_row
    }

    /// Natural column widths, shrunk proportionally (cells then wrap) when the table is too wide.
    fn column_widths(&self, table: &Table, columns: usize, available: f32) -> Vec<f32> {
        let mut widths = vec![TABLE_MIN_COLUMN_WIDTH; columns];
        let rows =
            std::iter::once((&table.header, true)).chain(table.rows.iter().map(|row| (row, false)));
        for (cells, is_header) in rows {
            for (column, cell) in cells.iter().enumerate().take(columns) {
                let (job, _) = inline_job(cell, self.body_size, is_header, f32::INFINITY);
                let natural =
                    self.ui.painter().layout_job(job).size().x + TABLE_CELL_PADDING.x * 2.0;
                widths[column] = widths[column].max(natural);
            }
        }
        let total: f32 = widths.iter().sum();
        if total > available {
            let scale = available / total;
            for width in &mut widths {
                *width = (*width * scale).max(TABLE_MIN_COLUMN_WIDTH);
            }
        }
        widths
    }

    fn grid(&mut self, left: f32, right: f32, row_tops: &[f32], widths: &[f32]) {
        let (Some(&top), Some(&bottom)) = (row_tops.first(), row_tops.last()) else {
            return;
        };
        for &y in row_tops {
            self.line(Pos2::new(left, y), Pos2::new(right, y));
        }
        let mut x = left;
        self.line(Pos2::new(x, top), Pos2::new(x, bottom));
        for width in widths {
            x += width;
            self.line(Pos2::new(x, top), Pos2::new(x, bottom));
        }
    }

    fn line(&mut self, from: Pos2, to: Pos2) {
        self.decorations.push(Decoration::Line {
            from,
            to,
            stretch: false,
        });
    }

    fn marker(&mut self, marker: Marker, depth: u8, left: f32, top: f32, line_height: f32) {
        let center_y = top + line_height / 2.0;
        let column_center = left - (LIST_INDENT + MARKER_GAP) / 2.0;
        match marker {
            Marker::Bullet => self.decorations.push(Decoration::Bullet {
                center: Pos2::new(column_center, center_y),
                // Alternates per level like most renderers: •, ◦, •, …
                filled: depth % 2 == 1,
            }),
            Marker::Task { checked } => self.decorations.push(Decoration::Checkbox {
                rect: Rect::from_center_size(
                    Pos2::new(column_center, center_y),
                    Vec2::splat(CHECKBOX_EDGE),
                ),
                checked,
            }),
            Marker::Ordered(number) => {
                let galley = self.ui.painter().layout_no_wrap(
                    format!("{number}."),
                    FontId::proportional(self.body_size),
                    theme::TEXT,
                );
                let x = (left - MARKER_GAP - galley.size().x).max(0.0);
                let y = center_y - galley.size().y / 2.0;
                self.texts.push(PlacedText {
                    offset: Vec2::new(x, y),
                    galley,
                    links: Vec::new(),
                });
            }
        }
    }

    /// One bar per quote level, joined to the previous block's bar across the gap between them.
    fn quote_bars(&mut self, block: &Block, previous: Option<&Block>, top: f32) {
        for level in 0..block.quote_depth {
            let joins_previous = previous.is_some_and(|previous| previous.quote_depth > level);
            let bar_top = if joins_previous {
                top - block_gap(previous.expect("checked above"), block)
            } else {
                top
            };
            let x = f32::from(level) * QUOTE_INDENT;
            self.decorations.push(Decoration::Fill {
                rect: Rect::from_min_max(
                    Pos2::new(x, bar_top),
                    Pos2::new(x + QUOTE_BAR_WIDTH, self.y),
                ),
                color: theme::ACCENT_LIGHT,
                radius: CornerRadius::ZERO,
                stretch: false,
            });
        }
    }

    fn place(&mut self, offset: Vec2, galley: Arc<Galley>, links: Vec<TextLink>) {
        let size = galley.size();
        self.width = self.width.max(offset.x + size.x);
        self.y = self.y.max(offset.y + size.y);
        self.texts.push(PlacedText {
            offset,
            galley,
            links,
        });
    }

    /// A block drawn as a box: the time cannot share its last line.
    fn close_box(&mut self, rect: Rect) {
        self.width = self.width.max(rect.right());
        self.y = self.y.max(rect.bottom());
        self.last_row_width = self.max_width;
    }

    fn finish(mut self) -> Rendered {
        let right = self.width;
        for decoration in &mut self.decorations {
            match decoration {
                Decoration::Fill {
                    rect,
                    stretch: true,
                    ..
                } => rect.max.x = rect.max.x.max(right),
                Decoration::Line {
                    to, stretch: true, ..
                } => to.x = right,
                _ => {}
            }
        }
        Rendered {
            texts: self.texts,
            decorations: self.decorations,
            size: Vec2::new(self.width, self.y),
            last_row_width: self.last_row_width,
        }
    }
}

/// One layout job for styled text, plus its links as byte ranges of the job's text.
fn inline_job(inline: &Inline, size: f32, bold: bool, wrap: f32) -> (LayoutJob, Vec<TextLink>) {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap;
    job.break_on_newline = true;
    if inline.text.is_empty() {
        job.append(
            EMPTY_LINE_TEXT,
            0.0,
            TextFormat::simple(FontId::proportional(size), theme::TEXT),
        );
        return (job, Vec::new());
    }
    let mut links = Vec::new();
    for run in &inline.runs {
        let style = run.style;
        let color = if run.link.is_some() {
            theme::ACCENT_LIGHT
        } else {
            theme::TEXT
        };
        let font_id = if style.code {
            FontId::monospace(size * CODE_FONT_SCALE)
        } else if bold || style.bold {
            FontId::new(size, bold_family())
        } else {
            FontId::new(size, FontFamily::Proportional)
        };
        let mut format = TextFormat::simple(font_id, color);
        format.italics = style.italic;
        if style.strike {
            format.strikethrough = Stroke::new(LINE_WIDTH, color);
        }
        if style.code {
            format.background = CODE_BG;
        }
        if let Some(url) = &run.link {
            format.underline = Stroke::new(LINE_WIDTH, color);
            links.push(TextLink {
                start: run.range.start,
                end: run.range.end,
                url: url.clone(),
            });
        }
        job.append(&inline.text[run.range.clone()], 0.0, format);
    }
    (job, links)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::markdown::parse::parse;

    const BODY: f32 = 15.0;
    const MAX_WIDTH: f32 = 300.0;

    fn rendered(source: &str) -> Rendered {
        let ctx = egui::Context::default();
        crate::ui::theme::apply(&ctx);
        let mut result = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            result = Some(layout(ui, &parse(source), BODY, MAX_WIDTH));
        });
        output.textures_delta.clear();
        result.expect("laid out")
    }

    #[test]
    fn bold_text_draws_with_the_bundled_face() {
        let out = rendered("**bold** and *italic* 😀");
        let galley = &out.texts[0].galley;
        assert_eq!(galley.text(), "bold and italic 😀");
        let glyphs: usize = galley.rows.iter().map(|row| row.glyphs.len()).sum();
        assert!(
            glyphs >= galley.text().chars().count() - 1,
            "missing glyphs"
        );
        assert!(out.last_row_width < MAX_WIDTH);
    }

    #[test]
    fn a_trailing_box_puts_the_time_on_its_own_line() {
        for source in [
            "text\n\n```\ncode\n```",
            "a\n\n---",
            "| a | b |\n|---|---|\n| 1 | 2 |",
        ] {
            assert_eq!(rendered(source).last_row_width, MAX_WIDTH, "{source:?}");
        }
    }

    #[test]
    fn content_stays_within_the_bubble() {
        let wide_table = format!(
            "| {} | {} |\n|---|---|\n| x | y |",
            "long ".repeat(30),
            "wide ".repeat(30)
        );
        for source in [
            wide_table.as_str(),
            "> > - [ ] deep\n>   nested quote with more words than fit",
        ] {
            let out = rendered(source);
            assert!(
                out.size.x <= MAX_WIDTH + 0.5,
                "{source:?} is {} wide",
                out.size.x
            );
            assert!(out.size.y > 0.0);
        }
    }

    #[test]
    fn blocks_stack_downwards() {
        let out = rendered("# Title\n\nbody\n\n1. one\n2. two");
        let tops: Vec<f32> = out
            .texts
            .iter()
            .filter(|text| !text.galley.text().ends_with('.'))
            .map(|text| text.offset.y)
            .collect();
        assert!(tops.windows(2).all(|pair| pair[0] < pair[1]), "{tops:?}");
    }
}
