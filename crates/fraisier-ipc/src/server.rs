//! Server-side helpers for building a first-party `fraisier-adapter-<name>` binary.
//!
//! A first-party adapter binary reads one `Content-Length`-framed JSON-RPC request
//! from stdin, dispatches it to its in-process adapter, and writes one framed
//! response to stdout, then exits — the one-shot model the client
//! ([`IpcMigrationAdapter`](crate::IpcMigrationAdapter) /
//! [`IpcArtifactAdapter`](crate::IpcArtifactAdapter)) drives, whether launched
//! locally or on a host over `ssh`. These helpers implement the framing and the
//! JSON-RPC envelope so such a binary is essentially a `match` on the method.
//!
//! Third-party adapters in *other languages* implement `PROTOCOL.md` directly (as
//! the reference sqlx adapter does, to keep the protocol language-agnostic);
//! first-party Rust binaries reuse this module.

use std::io::{self, Read, Write};

use fraisier_core::adapter_axes::AdapterError;
use serde::Deserialize;
use serde_json::Value;

/// The header/body separator.
const SEPARATOR: &[u8] = b"\r\n\r\n";

/// A decoded JSON-RPC request. `params` is left as a [`Value`] for the binary to
/// deserialize into its axis arguments (`ctx`, `host`, …).
#[derive(Debug, Deserialize)]
pub struct Request {
    /// Echoed verbatim into the response.
    #[serde(default)]
    pub id: Value,
    /// The method name (an axis trait method).
    pub method: String,
    /// The method arguments.
    #[serde(default)]
    pub params: Value,
}

/// Read every byte of `reader` (to EOF) and return the body of the single framed
/// message it contains.
///
/// # Errors
/// A message string if the buffer has no `Content-Length` header, an unparseable
/// length, or a truncated body (an underlying io error is rendered into it too).
pub fn read_framed(reader: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .map_err(|e| format!("failed to read stdin: {e}"))?;
    extract(&buf)
}

/// Extract the framed body from a complete buffer.
fn extract(buf: &[u8]) -> Result<Vec<u8>, String> {
    let header_end = buf
        .windows(SEPARATOR.len())
        .position(|w| w == SEPARATOR)
        .ok_or("no CRLFCRLF header terminator in input")?;
    let header = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| "non-UTF-8 bytes in message header".to_owned())?;

    let length: usize = header
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length:"))
        .ok_or("message has no Content-Length header")?
        .trim()
        .parse()
        .map_err(|_| "invalid Content-Length value".to_owned())?;

    let body_start = header_end + SEPARATOR.len();
    buf.get(body_start..body_start + length)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| {
            format!(
                "framed body is truncated: header declared {length} bytes, {} available",
                buf.len().saturating_sub(body_start)
            )
        })
}

/// Write `body` as one framed message to `writer` and flush.
///
/// # Errors
/// An io error if the write or flush fails.
pub fn write_framed(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()
}

/// Read and decode the one framed request from `reader`. On a framing/parse
/// failure, returns (`Err`) the framed JSON-RPC error response value to send back.
///
/// # Errors
/// The JSON-RPC `-32700` parse-error envelope (as a [`Value`]) when the input is
/// not a single framed JSON-RPC request.
pub fn read_request(reader: &mut impl Read) -> Result<Request, Value> {
    let body = read_framed(reader).map_err(|e| failure(Value::Null, -32700, &e))?;
    serde_json::from_slice(&body).map_err(|e| {
        failure(
            Value::Null,
            -32700,
            &format!("invalid JSON-RPC request: {e}"),
        )
    })
}

/// Echo a present id; default a missing/`null` id to `1` (the client always sends
/// `id: 1`, satisfying its response-id check).
#[must_use]
pub fn normalize_id(id: Value) -> Value {
    if id.is_null() {
        Value::from(1)
    } else {
        id
    }
}

/// Build a JSON-RPC response envelope with the given `member` (`result` or
/// `error`), echoing `id`.
fn envelope(id: Value, member: &str, value: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("jsonrpc".to_owned(), Value::from("2.0"));
    map.insert("id".to_owned(), id);
    map.insert(member.to_owned(), value);
    Value::Object(map)
}

/// Build the success response envelope, echoing `id`.
#[must_use]
pub fn success(id: Value, result: Value) -> Value {
    envelope(id, "result", result)
}

/// Build the error response envelope, echoing `id`.
#[must_use]
pub fn failure(id: Value, code: i64, message: &str) -> Value {
    let mut detail = serde_json::Map::new();
    detail.insert("code".to_owned(), Value::from(code));
    detail.insert("message".to_owned(), Value::from(message));
    envelope(id, "error", Value::Object(detail))
}

/// Map an [`AdapterError`] to the JSON-RPC error envelope, preserving its code and
/// surfacing any captured stderr as `error.data`.
#[must_use]
pub fn error_response(id: Value, error: &AdapterError) -> Value {
    let mut detail = serde_json::Map::new();
    detail.insert("code".to_owned(), Value::from(i64::from(error.code)));
    detail.insert("message".to_owned(), Value::from(error.message.clone()));
    if let Some(stderr) = &error.stderr {
        detail.insert("data".to_owned(), Value::from(stderr.clone()));
    }
    envelope(id, "error", Value::Object(detail))
}

#[cfg(test)]
mod tests {
    use super::{error_response, extract, normalize_id, read_request, success, write_framed};
    use fraisier_core::adapter_axes::AdapterError;
    use serde_json::Value;

    #[test]
    fn frames_round_trip() {
        let mut framed = Vec::new();
        write_framed(&mut framed, br#"{"a":1}"#).expect("write");
        assert!(framed.starts_with(b"Content-Length: 7\r\n\r\n"));
        assert_eq!(extract(&framed).expect("extract"), br#"{"a":1}"#);
    }

    #[test]
    fn read_request_parses_method_and_params() {
        let mut framed = Vec::new();
        write_framed(
            &mut framed,
            br#"{"jsonrpc":"2.0","id":1,"method":"stage","params":{"host":"web-1"}}"#,
        )
        .expect("write");
        let mut cursor = std::io::Cursor::new(framed);
        let request = read_request(&mut cursor).expect("parses");
        assert_eq!(request.method, "stage");
        assert_eq!(request.params["host"], "web-1");
    }

    #[test]
    fn read_request_returns_a_framed_parse_error() {
        let mut cursor = std::io::Cursor::new(b"not framed".to_vec());
        let envelope = read_request(&mut cursor).expect_err("malformed input");
        assert_eq!(envelope["error"]["code"], -32700);
    }

    #[test]
    fn normalize_id_defaults_null_to_one() {
        assert_eq!(normalize_id(Value::Null), Value::from(1));
        assert_eq!(normalize_id(Value::from(7)), Value::from(7));
    }

    #[test]
    fn success_and_error_envelopes_echo_the_id() {
        assert_eq!(success(Value::from(1), Value::Null)["id"], Value::from(1));
        let err = AdapterError::method_not_supported("stage");
        let envelope = error_response(Value::from(1), &err);
        assert_eq!(envelope["error"]["code"], i64::from(err.code));
    }
}
