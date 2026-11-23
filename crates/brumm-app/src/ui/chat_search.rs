//! Search inside the open chat. The sidebar lists matches; choosing one scrolls the room to it.
//!
//! The open conversation is already in memory. Matching is case-insensitive and runs again only
//! when the query or the conversation changes, so a long note is not folded every frame.

use brumm_core::{
    api::ConversationId,
    api::{Direction, MessageView},
    wire::MessageId,
};

/// Newest matches kept for the sidebar. Older ones stay reachable by narrowing the query.
pub const MAX_HITS: usize = 100;
/// Characters of context kept on each side of a match in the sidebar snippet.
const SNIPPET_CHARS: usize = 48;
/// How much original text is case-folded at once. A long note is walked in these windows.
const FOLD_CHUNK: usize = 64 * 1024;
/// Frames spent correcting a jump after the target row is actually measured.
const MAX_JUMP_ATTEMPTS: u8 = 4;
/// User scroll smaller than this does not clear the highlight (height settle, sub-pixel).
const HIGHLIGHT_SCROLL_PX: f32 = 2.0;

/// One matching message, newest first, with the match already cut out of the body.
pub struct SearchHit {
    pub id: MessageId,
    pub sent_at_ms: u64,
    pub direction: Direction,
    /// Author's name in a group (`None` in a 1:1 chat).
    pub sender: Option<String>,
    /// Text before the match, with a leading ellipsis when the body was cut.
    pub before: String,
    /// The matched slice, in the message's own casing.
    pub matched: String,
    /// Text after the match, with a trailing ellipsis when the body was cut.
    pub after: String,
}

/// Borrowed hit list for one frame of the sidebar.
pub struct HitList<'a> {
    pub matches: &'a [SearchHit],
    pub truncated: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Pending {
    #[default]
    None,
    /// Scroll the message list to the newest row and stick there.
    Bottom,
    Message(MessageId),
}

/// View state for searching the open room. Inactive until the header icon (or Ctrl+F) starts it.
#[derive(Default)]
pub struct Session {
    active: bool,
    conversation: Option<ConversationId>,
    query: String,
    /// The sidebar field should take keyboard focus.
    focus: bool,
    highlighted: Option<MessageId>,
    pending: Pending,
    pending_attempts: u8,
    /// This frame moved the list because of a jump, so the highlight stays.
    jumped_this_frame: bool,
    last_offset: Option<f32>,
    cached_query: String,
    cached_stamp: u64,
    hits: Vec<SearchHit>,
    truncated: bool,
}

impl Session {
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn query_mut(&mut self) -> &mut String {
        &mut self.query
    }

    pub(crate) fn trimmed_query(&self) -> &str {
        self.query.trim()
    }

    /// Take the one-shot request to focus the sidebar field.
    pub(crate) fn take_focus(&mut self) -> bool {
        std::mem::take(&mut self.focus)
    }

    pub(crate) fn highlighted(&self) -> Option<MessageId> {
        self.highlighted
    }

    pub(crate) fn pending(&self) -> Pending {
        self.pending
    }

    /// Start a fresh search in `contact`. The contact-list filter is left untouched.
    pub(crate) fn open(&mut self, conversation: ConversationId) {
        let focus = true;
        *self = Self::default();
        self.active = true;
        self.conversation = Some(conversation);
        self.focus = focus;
    }

    /// Icon click: open this room, or leave search and restore the chat list.
    pub(crate) fn toggle(&mut self, conversation: ConversationId) {
        if self.active {
            self.close();
        } else {
            self.open(conversation);
        }
    }

    /// Ctrl+F: open, or focus the field again without clearing the query.
    pub(crate) fn open_or_focus(&mut self, conversation: ConversationId) {
        if self.active && self.conversation == Some(conversation) {
            self.focus = true;
        } else {
            self.open(conversation);
        }
    }

    pub(crate) fn close(&mut self) {
        *self = Self::default();
    }

    /// Leave search when the open room changes. The sidebar is showing that other room's hits otherwise.
    pub(crate) fn note_open_chat(&mut self, conversation: Option<ConversationId>) {
        if self.active && self.conversation != conversation {
            self.close();
        }
    }

    pub(crate) fn reveal(&mut self, id: MessageId) {
        self.highlighted = Some(id);
        self.pending = Pending::Message(id);
        self.pending_attempts = 0;
    }

    pub(crate) fn jump_to_latest(&mut self) {
        self.highlighted = None;
        self.pending = Pending::Bottom;
        self.pending_attempts = 0;
    }

    /// `measured` is true once the target row has been laid out, so its height is real.
    /// Returns whether the jump is done.
    pub(crate) fn note_jump_attempt(&mut self, measured: bool) -> bool {
        self.jumped_this_frame = true;
        self.pending_attempts = self.pending_attempts.saturating_add(1);
        if measured || self.pending_attempts >= MAX_JUMP_ATTEMPTS {
            self.pending = Pending::None;
            self.pending_attempts = 0;
            true
        } else {
            false
        }
    }

    pub(crate) fn finish_jump(&mut self) {
        self.jumped_this_frame = true;
        self.pending = Pending::None;
        self.pending_attempts = 0;
    }

    pub(crate) fn take_jumped(&mut self) -> bool {
        std::mem::take(&mut self.jumped_this_frame)
    }

    #[cfg(test)]
    pub(crate) fn stored_hits(&self) -> usize {
        self.hits.len()
    }

    /// Forget the highlight when the user scrolls the room themselves.
    pub(crate) fn note_offset(&mut self, offset: f32, from_jump: bool) {
        if !from_jump
            && self.highlighted.is_some()
            && self
                .last_offset
                .is_some_and(|prev| (prev - offset).abs() > HIGHLIGHT_SCROLL_PX)
        {
            self.highlighted = None;
        }
        self.last_offset = Some(offset);
    }

    pub(crate) fn hits<'a>(&'a mut self, messages: &[MessageView]) -> HitList<'a> {
        let stamp = conversation_stamp(messages);
        let query = self.query.trim().to_owned();
        if query == self.cached_query && self.cached_stamp == stamp {
            return HitList {
                matches: &self.hits,
                truncated: self.truncated,
            };
        }
        self.cached_query.clone_from(&query);
        self.cached_stamp = stamp;
        self.hits.clear();
        self.truncated = false;
        if !query.is_empty() {
            let needle = query.to_lowercase();
            for message in messages.iter().rev().filter(|message| !message.system) {
                let text = searchable(message);
                let Some((start, end)) = find_ignore_case(text, &needle) else {
                    continue;
                };
                if self.hits.len() == MAX_HITS {
                    self.truncated = true;
                    break;
                }
                let (before, matched, after) = snippet(text, start, end);
                self.hits.push(SearchHit {
                    id: message.id,
                    sent_at_ms: message.sent_at_ms,
                    direction: message.direction,
                    sender: message.sender.as_ref().map(|sender| sender.display_name.clone()),
                    before,
                    matched,
                    after,
                });
            }
        }
        HitList {
            matches: &self.hits,
            truncated: self.truncated,
        }
    }
}

fn conversation_stamp(messages: &[MessageView]) -> u64 {
    let mut stamp = messages.len() as u64;
    if let Some(first) = messages.first() {
        stamp ^= u64::from_le_bytes(first.id[..8].try_into().unwrap_or([0; 8]));
    }
    if let Some(last) = messages.last() {
        stamp ^= u64::from_le_bytes(last.id[..8].try_into().unwrap_or([0; 8])).rotate_left(17);
        stamp ^= last.text.len() as u64;
        stamp ^= last.sent_at_ms;
    }
    stamp
}

fn searchable(message: &MessageView) -> &str {
    if !message.text.is_empty() {
        message.text.as_str()
    } else {
        message
            .attachment
            .as_ref()
            .map(|attachment| attachment.name.as_str())
            .unwrap_or("")
    }
}

/// Byte range of `needle_lower` in `haystack`, comparing without case.
fn find_ignore_case(haystack: &str, needle_lower: &str) -> Option<(usize, usize)> {
    if needle_lower.is_empty() || haystack.is_empty() {
        return None;
    }
    if needle_lower.is_ascii() && haystack.is_ascii() {
        if haystack.len() < needle_lower.len() {
            return None;
        }
        let needle = needle_lower.as_bytes();
        return haystack
            .as_bytes()
            .windows(needle.len())
            .position(|window| window.eq_ignore_ascii_case(needle))
            .map(|start| (start, start + needle.len()));
    }
    if needle_lower.is_ascii()
        && let Some(found) = find_ascii_needle(haystack, needle_lower.as_bytes())
    {
        return Some(found);
    }
    find_folded(haystack, needle_lower)
}

/// ASCII needle inside a string that also contains non-ASCII bytes.
fn find_ascii_needle(haystack: &str, needle: &[u8]) -> Option<(usize, usize)> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let bytes = haystack.as_bytes();
    let mut index = 0;
    while index + needle.len() <= bytes.len() {
        if bytes[index..index + needle.len()].eq_ignore_ascii_case(needle)
            && haystack.is_char_boundary(index + needle.len())
        {
            return Some((index, index + needle.len()));
        }
        let width = haystack[index..].chars().next().map_or(1, char::len_utf8);
        index += width;
    }
    None
}

fn find_folded(haystack: &str, needle_lower: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    while offset < haystack.len() {
        let mut end = (offset + FOLD_CHUNK).min(haystack.len());
        if end < haystack.len() {
            end = haystack.floor_char_boundary(end);
        }
        if let Some((start, stop)) = find_in_folded_piece(&haystack[offset..end], needle_lower) {
            return Some((start + offset, stop + offset));
        }
        if end == haystack.len() {
            break;
        }
        let overlap = needle_lower.len().saturating_mul(4).clamp(1, end - offset);
        let mut next = end.saturating_sub(overlap);
        next = haystack.floor_char_boundary(next);
        if next <= offset {
            break;
        }
        offset = next;
    }
    None
}

fn find_in_folded_piece(text: &str, needle_lower: &str) -> Option<(usize, usize)> {
    let mut lower = String::with_capacity(text.len());
    let mut map = Vec::with_capacity(text.len());
    for (index, ch) in text.char_indices() {
        let before = lower.len();
        for folded in ch.to_lowercase() {
            lower.push(folded);
        }
        for _ in 0..(lower.len() - before) {
            map.push(index as u32);
        }
    }
    let start = lower.find(needle_lower)?;
    let end = start + needle_lower.len();
    if end == 0 || end > map.len() {
        return None;
    }
    let origin = map[start] as usize;
    let last = map[end - 1] as usize;
    let width = text[last..].chars().next()?.len_utf8();
    Some((origin, last + width))
}

fn snippet(text: &str, start: usize, end: usize) -> (String, String, String) {
    let before_at = retreat(text, start, SNIPPET_CHARS);
    let after_at = advance(text, end, SNIPPET_CHARS);
    let mut before = flatten(&text[before_at..start]);
    let matched = flatten(&text[start..end]);
    let mut after = flatten(&text[end..after_at]);
    if before_at > 0 {
        before.insert(0, '…');
    }
    if after_at < text.len() {
        after.push('…');
    }
    (before, matched, after)
}

fn retreat(text: &str, mut index: usize, chars: usize) -> usize {
    for _ in 0..chars {
        if index == 0 {
            break;
        }
        index = text.floor_char_boundary(index - 1);
    }
    index
}

fn advance(text: &str, mut index: usize, chars: usize) -> usize {
    for _ in 0..chars {
        if index >= text.len() {
            break;
        }
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        index += ch.len_utf8();
    }
    index
}

/// Newlines become spaces so a snippet stays one sidebar line. Edge spaces are kept.
fn flatten(text: &str) -> String {
    let lead = text.chars().next().is_some_and(char::is_whitespace);
    let trail = text.chars().next_back().is_some_and(char::is_whitespace);
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    if lead && !out.starts_with(' ') {
        out.insert(0, ' ');
    }
    if trail && !out.ends_with(' ') {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use brumm_core::api::{AttachmentView, DeliveryStatus};

    use super::*;

    fn searching(query: &str) -> Session {
        Session {
            query: query.to_owned(),
            ..Default::default()
        }
    }

    fn message(id: u8, direction: Direction, text: &str) -> MessageView {
        MessageView {
            id: [id; 16],
            conversation: ConversationId::Direct([1; 32]),
            direction,
            text: text.to_owned(),
            attachment: None,
            reactions: Default::default(),
            sent_at_ms: 1_700_000_000_000 + u64::from(id) * 1000,
            status: DeliveryStatus::Delivered,
            sender: None,
            system: false,
        }
    }

    #[test]
    fn matches_are_case_insensitive_newest_first_and_keep_original_case() {
        let messages = vec![
            message(1, Direction::Incoming, "nothing here"),
            message(2, Direction::Outgoing, "Hello from Bob"),
            message(3, Direction::Incoming, "say hello again"),
        ];
        let mut session = searching("HELLO");
        let hits = session.hits(&messages);
        assert_eq!(hits.matches.len(), 2);
        assert!(!hits.truncated);
        assert_eq!(hits.matches[0].id, [3; 16]);
        assert_eq!(hits.matches[0].matched, "hello");
        assert_eq!(hits.matches[1].matched, "Hello");
        assert!(hits.matches[1].before.is_empty());
        assert!(hits.matches[1].after.contains("Bob"));
    }

    #[test]
    fn blank_query_lists_nothing_and_file_names_are_searched() {
        let mut file = message(1, Direction::Outgoing, "");
        file.attachment = Some(AttachmentView {
            name: "Report.PDF".to_owned(),
            size: 12,
        });
        let messages = vec![file, message(2, Direction::Incoming, "unrelated")];
        assert!(searching("").hits(&messages).matches.is_empty());
        let mut session = searching("pdf");
        let hits = session.hits(&messages);
        assert_eq!(hits.matches.len(), 1);
        assert_eq!(hits.matches[0].matched, "PDF");
        assert_eq!(hits.matches[0].id, [1; 16]);
    }

    #[test]
    fn long_match_is_cut_with_ellipsis_and_unicode_still_matches() {
        let body = format!("{}Needle{}", "a".repeat(80), "b".repeat(80));
        let messages = vec![
            message(1, Direction::Incoming, &body),
            message(2, Direction::Incoming, "a café on the corner 😀"),
        ];
        let mut session = searching("needle");
        let hits = session.hits(&messages);
        assert_eq!(hits.matches.len(), 1);
        assert!(hits.matches[0].before.starts_with('…'));
        assert!(hits.matches[0].after.ends_with('…'));
        assert_eq!(hits.matches[0].matched, "Needle");

        session = searching("CAFÉ");
        let hits = session.hits(&messages);
        assert_eq!(hits.matches.len(), 1);
        assert_eq!(hits.matches[0].matched, "café");
    }

    #[test]
    fn only_the_newest_matches_are_kept() {
        let messages: Vec<_> = (1..=120)
            .map(|id| message(id, Direction::Incoming, "hi there"))
            .collect();
        let mut session = searching("hi");
        let hits = session.hits(&messages);
        assert_eq!(hits.matches.len(), MAX_HITS);
        assert!(hits.truncated);
        assert_eq!(hits.matches[0].id, [120; 16]);
    }

    #[test]
    fn ascii_needle_is_found_at_the_end_of_a_long_note() {
        let mut body = "a".repeat(200_000);
        body.push_str("xyzzy");
        let found = find_ignore_case(&body, "xyzzy").expect("needle at the end");
        assert_eq!(&body[found.0..found.1], "xyzzy");
    }

    #[test]
    fn folded_match_across_a_chunk_boundary_is_found() {
        let mut body = "é".repeat(FOLD_CHUNK);
        body.push_str("café");
        let found = find_ignore_case(&body, "café").expect("match past the first chunk");
        assert_eq!(&body[found.0..found.1], "café");
    }
}
