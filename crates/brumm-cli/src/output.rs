//! Printing results: compact JSON (one value per line) with `--json`, short lines for people
//! otherwise. Diagnostics go to stderr so stdout stays clean for pipes.

use std::io::{IsTerminal, Write};

use qrcode::{QrCode, render::unicode::Dense1x2};
use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub struct Output {
    pub json: bool,
}

impl Output {
    /// Prints `value` as JSON, or `human` for people.
    pub fn emit(&self, value: &Value, human: impl FnOnce(&Value) -> String) {
        let text = if self.json {
            value.to_string()
        } else {
            human(value)
        };
        if text.is_empty() {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{text}");
        let _ = stdout.flush();
    }

    pub fn note(&self, text: &str) {
        eprintln!("{text}");
    }
}

/// `[room] sender: text` (or the file name) for one message view.
pub fn message_line(message: &Value, room_name: &str) -> String {
    let who = match (message["direction"].as_str(), message["sender"]["name"].as_str()) {
        (Some("out"), _) => "you".to_owned(),
        (_, Some(sender)) => sender.to_owned(),
        _ => room_name.to_owned(),
    };
    let body = match message["file"]["name"].as_str() {
        Some(name) => format!("[file {name}, {} bytes]", message["file"]["size"]),
        None => message["text"].as_str().unwrap_or_default().to_owned(),
    };
    if message["system"].as_bool() == Some(true) {
        return format!("[{room_name}] {body}");
    }
    format!("[{room_name}] {who}: {body}")
}

pub fn qr(link: &str) -> Option<String> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    let code = QrCode::new(link.as_bytes()).ok()?;
    Some(code.render::<Dense1x2>().quiet_zone(true).build())
}

pub fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}
