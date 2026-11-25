//! Markdown text → [`Doc`]: a flat list of blocks, each with its quote and list nesting, and
//! styled runs that cover its text byte for byte. Pure (no egui), so the layout can stay dumb.
//!
//! Messenger rules on top of CommonMark (+ GFM tables, strikethrough, task lists): a single
//! newline is a line break, raw HTML is shown as typed, and an image is never fetched (its alt
//! text becomes a link to it). Bare URLs outside code are linked like in plain messages.

use std::ops::Range;

use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};

use crate::ui::links;

const LINE_BREAK: &str = "\n";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
}

/// A styled byte range of its [`Inline`]'s text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub range: Range<usize>,
    pub style: Style,
    pub link: Option<String>,
}

/// Text with contiguous runs: every byte belongs to exactly one run, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inline {
    pub text: String,
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Bullet,
    Ordered(u64),
    Task { checked: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub alignments: Vec<ColumnAlign>,
    pub header: Vec<Inline>,
    pub rows: Vec<Vec<Inline>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph(Inline),
    Heading { level: u8, text: Inline },
    Code(String),
    Rule,
    Table(Table),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub quote_depth: u8,
    /// 0 outside lists, 1 for a top-level item, and so on.
    pub list_depth: u8,
    /// Set on the first block of a list item only; later blocks of the item are indented alike.
    pub marker: Option<Marker>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Doc {
    pub blocks: Vec<Block>,
}

impl Doc {
    /// One unstyled paragraph that reads exactly like the source: plain layout draws it the same.
    pub fn is_plain(&self, source: &str) -> bool {
        let [block] = self.blocks.as_slice() else {
            return self.blocks.is_empty();
        };
        let BlockKind::Paragraph(inline) = &block.kind else {
            return false;
        };
        block.quote_depth == 0
            && block.list_depth == 0
            && block.marker.is_none()
            && inline.text == source
            && inline.runs.iter().all(|run| {
                // A bare URL is linked by the plain path too.
                let bare_or_none = run
                    .link
                    .as_ref()
                    .is_none_or(|url| inline.text[run.range.clone()] == *url);
                run.style == Style::default() && bare_or_none
            })
    }
}

pub fn parse(source: &str) -> Doc {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder::default();
    for event in Parser::new_ext(source, options) {
        builder.event(event);
    }
    builder.finish()
}

enum InlineTarget {
    Paragraph,
    Heading(u8),
    TableCell,
}

struct TableBuilder {
    alignments: Vec<ColumnAlign>,
    header: Vec<Inline>,
    rows: Vec<Vec<Inline>>,
    in_head: bool,
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    quote_depth: u8,
    /// Next number of each open list; `None` for a bullet list.
    lists: Vec<Option<u64>>,
    pending_marker: Option<Marker>,
    inline: Option<(InlineTarget, Inline)>,
    code: Option<String>,
    table: Option<TableBuilder>,
    bold: u16,
    italic: u16,
    strike: u16,
    links: Vec<String>,
    /// Text length when each open image started, to spot an empty alt text.
    images: Vec<usize>,
}

impl Builder {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some(code) = &mut self.code {
                    code.push_str(&text);
                } else {
                    self.push_text(&text, self.style(false));
                }
            }
            Event::Code(text) => self.push_text(&text, self.style(true)),
            Event::Html(text) | Event::InlineHtml(text) => {
                if let Some(code) = &mut self.code {
                    code.push_str(&text);
                } else {
                    self.push_text(&text, self.style(false));
                }
            }
            Event::SoftBreak | Event::HardBreak => self.push_text(LINE_BREAK, self.style(false)),
            Event::Rule => {
                self.flush_inline();
                self.push_block(BlockKind::Rule);
            }
            Event::TaskListMarker(checked) => self.pending_marker = Some(Marker::Task { checked }),
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push_text(&text, self.style(true));
            }
            Event::FootnoteReference(label) => self.push_text(&label, self.style(false)),
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.begin_inline(InlineTarget::Paragraph),
            Tag::Heading { level, .. } => self.begin_inline(InlineTarget::Heading(level as u8)),
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.quote_depth = self.quote_depth.saturating_add(1);
            }
            Tag::CodeBlock(_) => {
                self.flush_inline();
                self.code = Some(String::new());
            }
            Tag::List(first) => {
                self.flush_inline();
                self.lists.push(first);
            }
            Tag::Item => {
                self.flush_inline();
                let marker = match self.lists.last_mut() {
                    Some(Some(next)) => {
                        let number = *next;
                        *next += 1;
                        Marker::Ordered(number)
                    }
                    _ => Marker::Bullet,
                };
                self.pending_marker = Some(marker);
            }
            Tag::Table(alignments) => {
                self.flush_inline();
                self.table = Some(TableBuilder {
                    alignments: alignments.into_iter().map(column_align).collect(),
                    header: Vec::new(),
                    rows: Vec::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.inline = Some((InlineTarget::TableCell, Inline::default())),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => self.links.push(dest_url.into_string()),
            Tag::Image { dest_url, .. } => {
                self.images.push(self.inline_len());
                self.links.push(dest_url.into_string());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::HtmlBlock => self.flush_inline(),
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                let mut code = self.code.take().unwrap_or_default();
                if code.ends_with('\n') {
                    code.pop();
                }
                self.push_block(BlockKind::Code(code));
            }
            TagEnd::List(_) => {
                self.flush_inline();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush_inline();
                // An empty item still shows its marker.
                if self.pending_marker.is_some() {
                    self.push_block(BlockKind::Paragraph(Inline::default()));
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_head = false;
                }
            }
            TagEnd::TableCell => {
                let Some((InlineTarget::TableCell, cell)) = self.inline.take() else {
                    return;
                };
                let cell = autolink(cell);
                let Some(table) = &mut self.table else {
                    return;
                };
                if table.in_head {
                    table.header.push(cell);
                } else if let Some(row) = table.rows.last_mut() {
                    row.push(cell);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_block(BlockKind::Table(Table {
                        alignments: table.alignments,
                        header: table.header,
                        rows: table.rows,
                    }));
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::Image => {
                // Without alt text the URL itself is the visible link.
                let start = self.images.pop();
                if start == Some(self.inline_len())
                    && let Some(url) = self.links.last().cloned()
                {
                    self.push_text(&url, self.style(false));
                }
                self.links.pop();
            }
            _ => {}
        }
    }

    fn style(&self, code: bool) -> Style {
        Style {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            code,
        }
    }

    fn inline_len(&self) -> usize {
        self.inline
            .as_ref()
            .map_or(0, |(_, inline)| inline.text.len())
    }

    fn begin_inline(&mut self, target: InlineTarget) {
        self.flush_inline();
        self.inline = Some((target, Inline::default()));
    }

    /// Appends to the open inline, or opens a paragraph for text outside one (tight list items).
    fn push_text(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        let link = self.links.last().cloned();
        let (_, inline) = self
            .inline
            .get_or_insert_with(|| (InlineTarget::Paragraph, Inline::default()));
        let start = inline.text.len();
        inline.text.push_str(text);
        let end = inline.text.len();
        if let Some(last) = inline.runs.last_mut()
            && last.style == style
            && last.link == link
        {
            last.range.end = end;
            return;
        }
        inline.runs.push(Run {
            range: start..end,
            style,
            link,
        });
    }

    fn flush_inline(&mut self) {
        let Some((target, inline)) = self.inline.take() else {
            return;
        };
        let inline = autolink(trim_trailing_breaks(inline));
        match target {
            InlineTarget::Paragraph => self.push_block(BlockKind::Paragraph(inline)),
            InlineTarget::Heading(level) => {
                self.push_block(BlockKind::Heading {
                    level,
                    text: inline,
                });
            }
            // A cell is closed by its own end tag; anything else ending it would lose it.
            InlineTarget::TableCell => self.inline = Some((InlineTarget::TableCell, inline)),
        }
    }

    fn push_block(&mut self, kind: BlockKind) {
        self.blocks.push(Block {
            kind,
            quote_depth: self.quote_depth,
            list_depth: u8::try_from(self.lists.len()).unwrap_or(u8::MAX),
            marker: self.pending_marker.take(),
        });
    }

    fn finish(mut self) -> Doc {
        self.flush_inline();
        if let Some(code) = self.code.take() {
            self.push_block(BlockKind::Code(code));
        }
        Doc {
            blocks: self.blocks,
        }
    }
}

fn column_align(alignment: Alignment) -> ColumnAlign {
    match alignment {
        Alignment::Center => ColumnAlign::Center,
        Alignment::Right => ColumnAlign::Right,
        Alignment::None | Alignment::Left => ColumnAlign::Left,
    }
}

fn trim_trailing_breaks(mut inline: Inline) -> Inline {
    let trimmed = inline.text.trim_end_matches('\n').len();
    if trimmed == inline.text.len() {
        return inline;
    }
    inline.text.truncate(trimmed);
    inline.runs.retain_mut(|run| {
        run.range.end = run.range.end.min(trimmed);
        run.range.start < run.range.end
    });
    inline
}

/// Splits unlinked, non-code runs around the bare URLs they contain.
fn autolink(inline: Inline) -> Inline {
    let mut runs = Vec::with_capacity(inline.runs.len());
    for run in inline.runs {
        if run.link.is_some() || run.style.code {
            runs.push(run);
            continue;
        }
        let base = run.range.start;
        let mut cursor = base;
        for found in links::find_links(&inline.text[run.range.clone()]) {
            let (start, end) = (base + found.start, base + found.end);
            if start > cursor {
                runs.push(Run {
                    range: cursor..start,
                    style: run.style,
                    link: None,
                });
            }
            runs.push(Run {
                range: start..end,
                style: run.style,
                link: Some(found.url),
            });
            cursor = end;
        }
        if cursor < run.range.end {
            runs.push(Run {
                range: cursor..run.range.end,
                style: run.style,
                link: None,
            });
        }
    }
    Inline {
        text: inline.text,
        runs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paragraph(doc: &Doc, index: usize) -> &Inline {
        match &doc.blocks[index].kind {
            BlockKind::Paragraph(inline) => inline,
            other => panic!("block {index} is {other:?}"),
        }
    }

    fn styled(inline: &Inline, style: Style) -> Vec<&str> {
        inline
            .runs
            .iter()
            .filter(|run| run.style == style)
            .map(|run| &inline.text[run.range.clone()])
            .collect()
    }

    #[test]
    fn plain_text_stays_plain() {
        for text in [
            "hello",
            "two\nlines",
            "see https://example.com now",
            "#hashtag",
            "1+1=2",
        ] {
            assert!(parse(text).is_plain(text), "{text:?}");
        }
        assert!(!parse("**bold**").is_plain("**bold**"));
        assert!(!parse("- item").is_plain("- item"));
    }

    #[test]
    fn inline_styles_hide_markers() {
        let doc = parse("a **b** *c* ~~d~~ `e`");
        let inline = paragraph(&doc, 0);
        assert_eq!(inline.text, "a b c d e");
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        assert_eq!(styled(inline, bold), ["b"]);
        assert_eq!(
            styled(
                inline,
                Style {
                    code: true,
                    ..Style::default()
                }
            ),
            ["e"]
        );
        let covered: usize = inline.runs.iter().map(|run| run.range.len()).sum();
        assert_eq!(covered, inline.text.len());
    }

    #[test]
    fn single_newline_is_a_line_break() {
        let doc = parse("one\ntwo");
        assert_eq!(paragraph(&doc, 0).text, "one\ntwo");
    }

    #[test]
    fn links_and_bare_urls() {
        let doc = parse("[site](https://a.example) and https://b.example");
        let inline = paragraph(&doc, 0);
        let links: Vec<_> = inline
            .runs
            .iter()
            .filter_map(|run| Some((&inline.text[run.range.clone()], run.link.as_deref()?)))
            .collect();
        assert_eq!(
            links,
            [
                ("site", "https://a.example"),
                ("https://b.example", "https://b.example")
            ]
        );
    }

    #[test]
    fn code_is_never_linked() {
        let doc = parse("`https://a.example`");
        assert!(paragraph(&doc, 0).runs.iter().all(|run| run.link.is_none()));
    }

    #[test]
    fn lists_nest_and_number() {
        let doc = parse("1. one\n2. two\n   - inner\n- [x] done");
        let markers: Vec<_> = doc
            .blocks
            .iter()
            .map(|block| (block.list_depth, block.marker))
            .collect();
        assert_eq!(
            markers,
            [
                (1, Some(Marker::Ordered(1))),
                (1, Some(Marker::Ordered(2))),
                (2, Some(Marker::Bullet)),
                (1, Some(Marker::Task { checked: true })),
            ]
        );
        assert_eq!(paragraph(&doc, 3).text, "done");
    }

    #[test]
    fn blocks_keep_their_kind_and_quote_depth() {
        let doc = parse("# Title\n\n> quoted\n>> deeper\n\n```rust\nfn x() {}\n```\n\n---");
        assert!(
            matches!(&doc.blocks[0].kind, BlockKind::Heading { level: 1, text } if text.text == "Title")
        );
        assert_eq!(doc.blocks[1].quote_depth, 1);
        assert_eq!(doc.blocks[2].quote_depth, 2);
        assert_eq!(doc.blocks[3].kind, BlockKind::Code("fn x() {}".into()));
        assert_eq!(doc.blocks[3].quote_depth, 0);
        assert_eq!(doc.blocks[4].kind, BlockKind::Rule);
    }

    #[test]
    fn tables_collect_header_rows_and_alignment() {
        let doc = parse("| a | b |\n|:--|--:|\n| 1 | **2** |");
        let BlockKind::Table(table) = &doc.blocks[0].kind else {
            panic!("{:?}", doc.blocks[0]);
        };
        assert_eq!(table.alignments, [ColumnAlign::Left, ColumnAlign::Right]);
        assert_eq!(
            table
                .header
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(table.rows.len(), 1);
        assert!(table.rows[0][1].runs[0].style.bold);
    }

    #[test]
    fn html_is_shown_as_typed_and_images_are_links() {
        let doc = parse("<b>hi</b> ![cat](https://img.example/cat.png)");
        let inline = paragraph(&doc, 0);
        assert!(inline.text.starts_with("<b>hi</b>"));
        assert!(
            inline
                .runs
                .iter()
                .any(|run| run.link.as_deref() == Some("https://img.example/cat.png"))
        );
    }

    #[test]
    fn empty_item_keeps_its_marker() {
        let doc = parse("-\n- b");
        assert_eq!(doc.blocks[0].marker, Some(Marker::Bullet));
        assert_eq!(paragraph(&doc, 0).text, "");
    }
}
