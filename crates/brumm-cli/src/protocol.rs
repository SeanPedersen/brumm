//! JSON-RPC 2.0 framing of the daemon protocol (see `protocol.md`): one JSON object per line.
//! Requests may arrive and be answered out of order; `id` pairs them. Subscriptions push
//! `event` notifications (no `id`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped on any incompatible change; clients refuse a daemon with another version.
pub const PROTOCOL_VERSION: u32 = 1;
pub const JSONRPC: &str = "2.0";
pub const EVENT_METHOD: &str = "event";
/// Longest accepted line: a 10 MiB file in base64 plus the envelope.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    /// The core or the host reported a failure.
    pub const FAILED: i64 = -32000;
    /// No such room, profile, identity or message.
    pub const NOT_FOUND: i64 = -32001;
    /// A room name that fits several rooms.
    pub const AMBIGUOUS: i64 = -32002;
    /// Refused by the profile's policy (lock mode).
    pub const REFUSED: i64 = -32003;
    /// Waited too long (e.g. for Delivered); `data` says how far it got.
    pub const TIMEOUT: i64 = -32004;
    /// The profile is in use by another process, or too many profiles are open.
    pub const BUSY: i64 = -32005;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    /// Absent for notifications from the client (answered with nothing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn new(id: Value, outcome: Result<Value, RpcError>) -> Self {
        let (result, error) = match outcome {
            Ok(result) => (Some(result), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            jsonrpc: JSONRPC.to_owned(),
            id,
            result,
            error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn failed(err: impl std::fmt::Display) -> Self {
        Self::new(code::FAILED, err.to_string())
    }

    pub fn invalid_params(err: impl std::fmt::Display) -> Self {
        Self::new(code::INVALID_PARAMS, err.to_string())
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new(code::NOT_FOUND, what)
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(err: anyhow::Error) -> Self {
        Self::new(code::FAILED, format!("{err:#}"))
    }
}

/// An `event` notification line for subscription `subscription`.
pub fn event_line(subscription: u64, profile: &str, event: &impl Serialize) -> String {
    let notification = serde_json::json!({
        "jsonrpc": JSONRPC,
        "method": EVENT_METHOD,
        "params": { "subscription": subscription, "profile": profile, "event": event },
    });
    notification.to_string()
}

/// Reads one line (without the newline) of at most [`MAX_LINE_BYTES`]; `None` at the end of
/// the stream. A longer line is an error: the connection cannot resynchronise after it.
pub async fn read_line<R>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<()>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    buf.clear();
    let read = (&mut *reader)
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', buf)
        .await?;
    if read == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() > MAX_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("a line is longer than {MAX_LINE_BYTES} bytes"),
        ));
    }
    Ok(Some(()))
}
