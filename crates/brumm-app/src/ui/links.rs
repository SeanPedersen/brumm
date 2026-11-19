//! URLs in message text. Detection only: the bubble paints them and the system opens them.
//! A long note is scanned only around the lines on screen.

/// How far a visible slice looks past its edge for a URL that wraps across the cut.
const LINK_LOOKAROUND: usize = 2048;

/// A URL inside a laid-out string. `start..end` is a byte range of that string.
#[derive(Debug, PartialEq, Eq)]
pub struct TextLink {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

/// Every web or mail link in `text`, in order, without overlaps.
pub fn find_links(text: &str) -> Vec<TextLink> {
    let mut links = Vec::new();
    let mut boundary = true;
    let mut index = 0;
    while index < text.len() {
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        if boundary && let Some((end, url)) = match_link(text, index) {
            links.push(TextLink {
                start: index,
                end,
                url,
            });
            index = end;
            boundary = false;
            continue;
        }
        boundary = is_link_boundary(ch);
        index += ch.len_utf8();
    }
    links
}

/// Links that overlap `text[from..to]`. Ranges are rewritten into that slice, so they match
/// the galley built from it. The opened URL still includes the part that sits just outside.
pub fn links_in_slice(text: &str, from: usize, to: usize) -> Vec<TextLink> {
    if from >= to || to > text.len() || !text.is_char_boundary(from) || !text.is_char_boundary(to) {
        return Vec::new();
    }
    let scan_from = rewind(text, from);
    let scan_to = extend(text, to);
    let base = scan_from;
    find_links(&text[scan_from..scan_to])
        .into_iter()
        .filter_map(|link| {
            let abs_start = base + link.start;
            let abs_end = base + link.end;
            if abs_end <= from || abs_start >= to {
                return None;
            }
            let start = abs_start.saturating_sub(from);
            let end = (abs_end - from).min(to - from);
            if start >= end {
                return None;
            }
            Some(TextLink {
                start,
                end,
                url: link.url,
            })
        })
        .collect()
}

fn rewind(text: &str, from: usize) -> usize {
    let floor = from.saturating_sub(LINK_LOOKAROUND);
    let mut index = from;
    while index > floor {
        let Some(ch) = text[..index].chars().next_back() else {
            break;
        };
        let prev = index - ch.len_utf8();
        if prev < floor || is_link_boundary(ch) {
            break;
        }
        index = prev;
    }
    index
}

fn extend(text: &str, to: usize) -> usize {
    let ceil = (to + LINK_LOOKAROUND).min(text.len());
    let mut index = to;
    while index < ceil {
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        if ch.is_whitespace() || ch.is_control() {
            break;
        }
        let next = index + ch.len_utf8();
        if next > ceil {
            break;
        }
        index = next;
    }
    index
}

fn is_link_boundary(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '(' | '[' | '{' | '<' | '"' | '\'' | '«' | '“' | '‘' | '「'
        )
}

fn match_link(text: &str, start: usize) -> Option<(usize, String)> {
    let rest = &text[start..];
    let (scheme, prefix) = if has_ascii_prefix(rest, "https://") {
        ("https://", None)
    } else if has_ascii_prefix(rest, "http://") {
        ("http://", None)
    } else if has_ascii_prefix(rest, "mailto:") {
        ("mailto:", None)
    } else if has_ascii_prefix(rest, "www.") {
        ("www.", Some("https://"))
    } else {
        return None;
    };
    let scheme_len = scheme.len();
    let mut end = start + scheme_len;
    while end < text.len() {
        let ch = text[end..].chars().next()?;
        if ch.is_whitespace() || ch.is_control() || matches!(ch, '<' | '>' | '"' | '`') {
            break;
        }
        end += ch.len_utf8();
    }
    end = trim_trailing_punctuation(text, start, end);
    if end <= start + scheme_len {
        return None;
    }
    let raw = &text[start..end];
    let body = &raw[scheme_len..];
    if prefix.is_some() {
        let dot = body.find('.')?;
        if dot + 1 >= body.len() {
            return None;
        }
    } else if has_ascii_prefix(raw, "mailto:") {
        let at = body.find('@')?;
        if at == 0 || at + 1 >= body.len() {
            return None;
        }
    } else if body.is_empty() || body.bytes().all(|byte| byte == b'/') {
        return None;
    }
    let url = match prefix {
        Some(prefix) => format!("{prefix}{raw}"),
        None => raw.to_owned(),
    };
    Some((end, url))
}

fn has_ascii_prefix(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Drops sentence punctuation and a closing bracket that has no opener inside the URL.
fn trim_trailing_punctuation(text: &str, start: usize, mut end: usize) -> usize {
    let bytes = text.as_bytes();
    while end > start {
        let byte = bytes[end - 1];
        if matches!(byte, b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"') {
            end -= 1;
            continue;
        }
        if matches!(byte, b')' | b']' | b'}') {
            let open = match byte {
                b')' => b'(',
                b']' => b'[',
                _ => b'{',
            };
            let slice = &bytes[start..end];
            let opens = slice.iter().filter(|candidate| **candidate == open).count();
            let closes = slice.iter().filter(|candidate| **candidate == byte).count();
            if closes > opens {
                end -= 1;
                continue;
            }
        }
        break;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(text: &str) -> Vec<String> {
        find_links(text).into_iter().map(|link| link.url).collect()
    }

    #[test]
    fn finds_web_and_mail_links_and_drops_sentence_punctuation() {
        assert_eq!(
            urls("see https://example.com/a. next"),
            vec!["https://example.com/a".to_owned()]
        );
        assert_eq!(
            urls("go (https://example.com/a) now"),
            vec!["https://example.com/a".to_owned()]
        );
        assert_eq!(
            urls("path https://example.com/a(b) end"),
            vec!["https://example.com/a(b)".to_owned()]
        );
        assert_eq!(
            urls("WWW.Example.com/x and mailto:a@b.c"),
            vec![
                "https://WWW.Example.com/x".to_owned(),
                "mailto:a@b.c".to_owned()
            ]
        );
        assert!(urls("notwww.example.com").is_empty());
        assert!(urls("http://").is_empty());
        assert!(urls("mailto:nobody").is_empty());
    }

    #[test]
    fn a_slice_that_starts_mid_url_still_opens_the_whole_link() {
        let mut text = "x".repeat(40);
        text.push_str(" https://example.com/path");
        let from = text.find("example").expect("host");
        let links = links_in_slice(&text, from, text.len());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://example.com/path");
        assert_eq!(links[0].start, 0);
        assert_eq!(&text[from..from + links[0].end], "example.com/path");
    }
}
