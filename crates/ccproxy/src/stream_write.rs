//! Writing a streaming response to the client socket, one flush per event.
//!
//! This replaces tiny_http's `Request::respond` for SSE bodies only, and it
//! exists because of a buffering artifact in tiny_http 0.12's write path:
//! `raw_print` copies the body through a `chunked_transfer::Encoder` wrapped
//! around a **1024-byte `BufWriter`**, and nothing flushes until the response
//! is complete. For a stream that means the client sees nothing for seconds at
//! a time and then a burst of about a kilobyte: the buffer only drains when it
//! fills, so the inter-burst gap is exactly "how long the model needed to
//! generate another kilobyte of SSE". The first byte still looks fast, because
//! the status line plus the opening events cross 1024 bytes quickly — which is
//! precisely the reported symptom: fast TTFB, then large periodic chunks with
//! long quiet stretches between them.
//!
//! `Request::into_writer` is the documented escape hatch for raw control of
//! the socket, and it hands back the very writer `respond` would have used.
//! `SequentialWriter::flush` forwards to that same `BufWriter`, which forwards
//! to the socket — so writing the response by hand and flushing per event
//! gives true event-at-a-time delivery while keeping tiny_http's connection
//! handling (request parsing, keep-alive, pipelining) untouched.
//!
//! Only HTTP/1.1 is handled here, because chunked framing requires it. An
//! HTTP/1.0 client keeps the legacy `respond` path, which — lacking a content
//! length — buffers the whole body; that is the same behavior as before this
//! module existed, so nothing regresses on a path no SSE client takes.

use std::io::{self, Read, Write};
use tiny_http::{HTTPVersion, Header, StatusCode};

/// Whether [`write_chunked_response`] may be used for this request version.
pub fn supports_chunked(version: &HTTPVersion) -> bool {
    *version >= (1, 1)
}

/// Write `status` + `headers` + a chunked transfer of `body` to `writer`,
/// flushing the underlying socket after the header block and after every body
/// read, so an event reaches the client when the upstream produced it rather
/// than when a write buffer happens to fill.
///
/// Returns when `body` is exhausted (the terminal chunk has been written) or
/// the client is gone. A read error from `body` propagates and — deliberately —
/// leaves the chunked body unterminated: a mid-stream failure must never look
/// like a clean end of stream, so no `0\r\n\r\n` is written after it.
pub fn write_chunked_response<W: Write>(
    writer: &mut W,
    version: &HTTPVersion,
    status: StatusCode,
    headers: &[Header],
    body: &mut dyn Read,
) -> io::Result<()> {
    // Status line — the same shape tiny_http's own `write_message_header`
    // emits, so the wire is identical apart from the flush cadence.
    write!(
        writer,
        "HTTP/{}.{} {} {}\r\n",
        version.0,
        version.1,
        status.0,
        status.default_reason_phrase()
    )?;
    for header in headers {
        writer.write_all(header.field.as_str().as_ref())?;
        writer.write_all(b": ")?;
        writer.write_all(header.value.as_str().as_ref())?;
        writer.write_all(b"\r\n")?;
    }
    writer.write_all(b"Transfer-Encoding: chunked\r\n")?;
    writer.write_all(b"\r\n")?;
    // The status line and headers must not wait for the first event: the
    // client's spinner starts here.
    writer.flush()?;

    let mut buf = [0u8; 8192];
    loop {
        let n = body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        write!(writer, "{:x}\r\n", n)?;
        if let Some(slice) = buf.get(..n) {
            writer.write_all(slice)?;
        }
        writer.write_all(b"\r\n")?;
        writer.flush()?;
    }
    // The zero-length chunk ends the body; nothing may follow it.
    writer.write_all(b"0\r\n\r\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Delegates writes to a Vec and counts `flush` calls, so a test can pin
    /// the flush cadence — the whole point of this module.
    struct CountingWriter {
        inner: Vec<u8>,
        flushes: usize,
    }

    impl CountingWriter {
        fn new() -> Self {
            Self {
                inner: Vec::new(),
                flushes: 0,
            }
        }
    }

    impl Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    /// Yields the given pieces one read at a time, then EOF — mirroring how
    /// `SseBody::read` returns one encoded event per call.
    struct PieceReader {
        pieces: Vec<Vec<u8>>,
    }

    impl PieceReader {
        fn new(pieces: &[&str]) -> Self {
            Self {
                pieces: pieces.iter().map(|p| p.as_bytes().to_vec()).collect(),
            }
        }
    }

    impl Read for PieceReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pieces.is_empty() {
                return Ok(0);
            }
            let piece = self.pieces.remove(0);
            let n = piece.len().min(buf.len());
            buf[..n].copy_from_slice(&piece[..n]);
            Ok(n)
        }
    }

    fn headers() -> Vec<Header> {
        vec![
            Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap(),
            Header::from_bytes(&b"Cache-Control"[..], &b"no-cache"[..]).unwrap(),
        ]
    }

    #[test]
    fn version_gate_matches_the_chunked_carrying_generation() {
        assert!(supports_chunked(&HTTPVersion(1, 1)));
        assert!(supports_chunked(&HTTPVersion(1, 2)));
        assert!(!supports_chunked(&HTTPVersion(1, 0)));
    }

    #[test]
    fn status_and_headers_precede_the_first_body_byte() {
        let mut w = CountingWriter::new();
        let mut body = PieceReader::new(&["data: hi\n\n"]);
        write_chunked_response(
            &mut w,
            &HTTPVersion(1, 1),
            StatusCode(200),
            &headers(),
            &mut body,
        )
        .unwrap();

        let out = String::from_utf8(w.inner).unwrap();
        let head_end = out.find("\r\n\r\n").expect("header terminator");
        let head = &out[..head_end];
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head:?}");
        assert!(
            head.contains("Content-Type: text/event-stream\r\n"),
            "{head:?}"
        );
        assert!(head.contains("Cache-Control: no-cache\r\n"), "{head:?}");
        assert!(head.contains("Transfer-Encoding: chunked"), "{head:?}");
        // First chunk frame directly follows the header block: "data: hi\n\n"
        // is 10 bytes, so its length prefix is the hex digit `a`.
        assert!(
            out[head_end + 4..].starts_with("a\r\ndata: hi\n\n\r\n"),
            "{out:?}"
        );
    }

    #[test]
    fn every_read_is_flushed_individually() {
        let mut w = CountingWriter::new();
        let mut body = PieceReader::new(&["one\n", "two\n", "three\n"]);
        write_chunked_response(
            &mut w,
            &HTTPVersion(1, 1),
            StatusCode(200),
            &headers(),
            &mut body,
        )
        .unwrap();
        // One flush for the header block, one per body read, one for the
        // terminal chunk. Anything fewer batches events client-side.
        assert_eq!(w.flushes, 1 + 3 + 1);
    }

    #[test]
    fn the_terminal_chunk_closes_the_body() {
        let mut w = CountingWriter::new();
        let mut body = PieceReader::new(&["x"]);
        write_chunked_response(
            &mut w,
            &HTTPVersion(1, 1),
            StatusCode(200),
            &headers(),
            &mut body,
        )
        .unwrap();
        assert!(String::from_utf8(w.inner).unwrap().ends_with("0\r\n\r\n"));
    }

    #[test]
    fn chunk_sizes_are_lowercase_hex_with_crlf_framing() {
        let mut w = CountingWriter::new();
        let payload = "abcdefghij"; // 10 bytes
        let mut body = Cursor::new(payload.as_bytes().to_vec());
        write_chunked_response(
            &mut w,
            &HTTPVersion(1, 1),
            StatusCode(200),
            &headers(),
            &mut body,
        )
        .unwrap();
        let out = String::from_utf8(w.inner).unwrap();
        assert!(out.contains("\r\na\r\nabcdefghij\r\n"), "{out:?}");
    }

    #[test]
    fn a_read_error_leaves_the_body_open() {
        struct BrokenAfterOne {
            first: bool,
        }
        impl Read for BrokenAfterOne {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    buf[0] = b'x';
                    return Ok(1);
                }
                Err(io::Error::other("upstream died"))
            }
        }

        let mut w = CountingWriter::new();
        let mut body = BrokenAfterOne { first: true };
        let result = write_chunked_response(
            &mut w,
            &HTTPVersion(1, 1),
            StatusCode(200),
            &headers(),
            &mut body,
        );
        assert!(result.is_err());
        let out = String::from_utf8(w.inner).unwrap();
        // The delivered chunk is on the wire, the body is not terminated: a
        // truncated stream must not read as a clean end.
        assert!(out.contains("1\r\nx\r\n"), "{out:?}");
        assert!(!out.ends_with("0\r\n\r\n"), "{out:?}");
    }
}
