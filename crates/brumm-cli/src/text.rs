//! Pure helpers for CLI input: durations (`500ms`, `30s`, `5m`, `1h`), splitting text that is
//! over the message limit, and ids for the parts of a split message.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use brumm_core::wire::MessageId;

const MILLIS_PER_SECOND: u64 = 1000;
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 3600;

/// `"250ms"`, `"30s"`, `"5m"`, `"2h"`; a bare number is seconds.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: u64 = number
        .parse()
        .with_context(|| format!("invalid duration {text:?}"))?;
    Ok(match unit {
        "ms" => Duration::from_millis(value),
        "" | "s" => Duration::from_secs(value),
        "m" => Duration::from_secs(value * SECONDS_PER_MINUTE),
        "h" => Duration::from_secs(value * SECONDS_PER_HOUR),
        _ => bail!("invalid duration unit in {text:?} (use ms, s, m or h)"),
    })
}

pub fn duration_millis(duration: Duration) -> u64 {
    duration.as_secs() * MILLIS_PER_SECOND + u64::from(duration.subsec_millis())
}

/// Splits `text` into parts of at most `limit` bytes, at the last line break of a part if
/// there is one, else at a char boundary. Line breaks at a cut are kept with the earlier part.
pub fn split_text(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text;
    while rest.len() > limit {
        let mut cut = limit;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(newline) = rest[..cut].rfind('\n') {
            cut = newline + 1;
        }
        parts.push(rest[..cut].to_owned());
        rest = &rest[cut..];
    }
    if !rest.is_empty() || parts.is_empty() {
        parts.push(rest.to_owned());
    }
    parts
}

/// Part `index` of a split message whose caller chose `id`: stable, so a retry of the whole
/// split send repeats the same ids.
pub fn part_id(id: &MessageId, index: usize) -> MessageId {
    let mut hasher = blake3::Hasher::new_derive_key("brumm-cli 2026 split message part");
    hasher.update(id);
    hasher.update(&(index as u64).to_le_bytes());
    let mut part = [0; 16];
    part.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    part
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert!(parse_duration("5d").is_err());
        assert!(parse_duration("m").is_err());
        assert_eq!(duration_millis(Duration::from_millis(1500)), 1500);
    }

    #[test]
    fn splits_at_lines_then_chars_and_rejoins() {
        let text = "aaaa\nbbbb\nccc";
        let parts = split_text(text, 7);
        assert_eq!(parts, vec!["aaaa\n", "bbbb\n", "ccc"]);
        assert_eq!(parts.concat(), text);
        let wide = "ééééé";
        let parts = split_text(wide, 3);
        assert!(parts.iter().all(|part| part.len() <= 3));
        assert_eq!(parts.concat(), wide);
        assert_eq!(split_text("short", 10), vec!["short"]);
    }

    #[test]
    fn part_ids_are_stable_and_distinct() {
        let id = [1; 16];
        assert_eq!(part_id(&id, 0), part_id(&id, 0));
        assert_ne!(part_id(&id, 0), part_id(&id, 1));
        assert_ne!(part_id(&id, 0), id);
    }
}
