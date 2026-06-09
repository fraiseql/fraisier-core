//! LSP-style `Content-Length` message framing over async byte streams.
//!
//! A message is `Content-Length: <n>\r\n\r\n` followed by exactly `<n>` bytes of
//! body. This lets multiple JSON-RPC messages share one stdio stream
//! unambiguously.

use std::io;

use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

/// Write `body` as one framed message and flush.
pub async fn write_message<W>(writer: &mut W, body: &[u8]) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one framed message. Returns `Ok(None)` on a clean EOF before any header
/// (the stream ended with no further message).
pub async fn read_message<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            // EOF. A clean end before any header is "no message"; a half-read
            // header block is malformed.
            return if content_length.is_none() && line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended mid-header",
                ))
            };
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // blank line terminates the header block
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length header")
            })?);
        }
        // Unknown headers are ignored, per the protocol.
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "message has no Content-Length header",
        )
    })?;
    let mut body = vec![0_u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::{read_message, write_message};
    use std::io::Cursor;

    #[tokio::test]
    async fn round_trips_a_framed_message() {
        let mut buf = Vec::new();
        write_message(&mut buf, b"{\"hello\":1}")
            .await
            .expect("write");

        let framed = String::from_utf8(buf.clone()).expect("utf8");
        assert!(framed.starts_with("Content-Length: 11\r\n\r\n"));

        let mut reader = Cursor::new(buf);
        let body = read_message(&mut reader)
            .await
            .expect("read")
            .expect("some");
        assert_eq!(body, b"{\"hello\":1}");
    }

    #[tokio::test]
    async fn clean_eof_is_none() {
        let mut reader = Cursor::new(Vec::new());
        assert!(read_message(&mut reader).await.expect("read").is_none());
    }
}
