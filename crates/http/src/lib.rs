// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! An HTTP/1.1 client that works over any byte stream: a TCP socket, a TLS
//! session, a Unix socket or a Windows named pipe.
//!
//! Each request uses its own connection and asks the server to close it
//! afterwards. That is slower than reusing connections, but there is no
//! connection state to get wrong, and nothing here is called often enough
//! for it to matter.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};

/// Longest status or header line accepted.
const MAX_LINE: usize = 16 * 1024;
/// Most header lines accepted in one response.
const MAX_HEADERS: usize = 200;
/// Size of the pieces a body of unknown length is sent in.
const CHUNK: usize = 64 * 1024;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The peer sent something that is not valid HTTP/1.1.
    Protocol(&'static str),
    /// The request itself cannot be sent, for example because a header
    /// contains a line break.
    BadRequest(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "HTTP I/O error: {e}"),
            Error::Protocol(message) => write!(f, "invalid HTTP response: {message}"),
            Error::BadRequest(message) => write!(f, "invalid HTTP request: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

/// The body of a request.
pub enum Body<'a> {
    Empty,
    Bytes(&'a [u8]),
    /// A body of known length, sent with `Content-Length`.
    Sized(&'a mut dyn Read, u64),
    /// A body of unknown length, sent with chunked transfer encoding.
    Chunked(&'a mut dyn Read),
}

pub struct Request<'a> {
    pub method: &'a str,
    /// The request target: a path, with a query string if there is one.
    pub target: &'a str,
    /// The value of the `Host` header.
    pub host: &'a str,
    pub headers: Vec<(&'a str, &'a str)>,
    pub body: Body<'a>,
}

impl<'a> Request<'a> {
    pub fn new(method: &'a str, target: &'a str, host: &'a str) -> Request<'a> {
        Request {
            method,
            target,
            host,
            headers: Vec::new(),
            body: Body::Empty,
        }
    }

    pub fn header(mut self, name: &'a str, value: &'a str) -> Request<'a> {
        self.headers.push((name, value));
        self
    }

    pub fn body(mut self, body: Body<'a>) -> Request<'a> {
        self.body = body;
        self
    }

    /// Sends the request on `stream` and reads the response head. The
    /// response body is then read from the returned [`Response`].
    pub fn send<S: Read + Write>(self, mut stream: S) -> Result<Response<S>, Error> {
        let is_head = self.method == "HEAD";
        self.write_to(&mut stream)?;
        Response::read(stream, is_head)
    }

    fn write_to(self, stream: &mut dyn Write) -> Result<(), Error> {
        if self.method.is_empty() || !self.method.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(Error::BadRequest("bad method"));
        }
        if self.target.is_empty() || self.target.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Err(Error::BadRequest(
                "target contains a space or control character",
            ));
        }
        check_header_value(self.host)?;

        let mut head = Vec::with_capacity(256);
        head.extend_from_slice(self.method.as_bytes());
        head.push(b' ');
        head.extend_from_slice(self.target.as_bytes());
        head.extend_from_slice(b" HTTP/1.1\r\nHost: ");
        head.extend_from_slice(self.host.as_bytes());
        head.extend_from_slice(b"\r\nConnection: close\r\n");
        for (name, value) in &self.headers {
            if name.is_empty() || !name.bytes().all(is_token_byte) {
                return Err(Error::BadRequest("bad header name"));
            }
            if ["host", "connection", "content-length", "transfer-encoding"]
                .iter()
                .any(|reserved| name.eq_ignore_ascii_case(reserved))
            {
                return Err(Error::BadRequest("header is set by the client itself"));
            }
            check_header_value(value)?;
            head.extend_from_slice(name.as_bytes());
            head.extend_from_slice(b": ");
            head.extend_from_slice(value.as_bytes());
            head.extend_from_slice(b"\r\n");
        }
        let carries_body = !matches!(self.method, "GET" | "HEAD" | "DELETE");
        match &self.body {
            Body::Empty => {
                if carries_body {
                    head.extend_from_slice(b"Content-Length: 0\r\n");
                }
            }
            Body::Bytes(bytes) => {
                head.extend_from_slice(format!("Content-Length: {}\r\n", bytes.len()).as_bytes())
            }
            Body::Sized(_, len) => {
                head.extend_from_slice(format!("Content-Length: {len}\r\n").as_bytes())
            }
            Body::Chunked(_) => head.extend_from_slice(b"Transfer-Encoding: chunked\r\n"),
        }
        head.extend_from_slice(b"\r\n");

        match self.body {
            Body::Empty => stream.write_all(&head)?,
            Body::Bytes(bytes) => {
                // One write for small requests, so they leave in one packet.
                head.extend_from_slice(bytes);
                stream.write_all(&head)?;
            }
            Body::Sized(reader, len) => {
                stream.write_all(&head)?;
                let sent = io::copy(&mut reader.take(len), stream)?;
                if sent != len {
                    return Err(Error::BadRequest("body was shorter than its stated length"));
                }
            }
            Body::Chunked(reader) => {
                stream.write_all(&head)?;
                let mut buf = vec![0u8; CHUNK];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(Error::Io(e)),
                    };
                    if n == 0 {
                        break;
                    }
                    stream.write_all(format!("{n:x}\r\n").as_bytes())?;
                    stream.write_all(&buf[..n])?;
                    stream.write_all(b"\r\n")?;
                }
                stream.write_all(b"0\r\n\r\n")?;
            }
        }
        stream.flush()?;
        Ok(())
    }
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn check_header_value(value: &str) -> Result<(), Error> {
    // A line break in a value would let it smuggle in another header.
    if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return Err(Error::BadRequest("header value contains a line break"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// No body.
    None,
    /// This many bytes remain.
    Length(u64),
    /// Between chunks: the next thing to read is a chunk-size line.
    ChunkStart,
    /// Inside a chunk with this many bytes remaining.
    ChunkData(u64),
    /// The body runs until the peer closes the connection.
    UntilClose,
    /// The body has been read to its end.
    Done,
}

pub struct Response<S: Read> {
    pub status: u16,
    pub reason: String,
    headers: Vec<(String, String)>,
    stream: BufReader<S>,
    framing: Framing,
}

impl<S: Read> fmt::Debug for Response<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("reason", &self.reason)
            .field("headers", &self.headers)
            .finish()
    }
}

impl<S: Read> Response<S> {
    fn read(stream: S, is_head: bool) -> Result<Response<S>, Error> {
        let mut stream = BufReader::new(stream);
        loop {
            let (status, reason) = read_status_line(&mut stream)?;
            let headers = read_headers(&mut stream)?;
            // Informational responses come before the real one.
            if (100..200).contains(&status) && status != 101 {
                continue;
            }
            let framing = if is_head || status == 204 || status == 304 || status == 101 {
                Framing::None
            } else {
                body_framing(&headers)?
            };
            return Ok(Response {
                status,
                reason,
                headers,
                stream,
                framing,
            });
        }
    }

    /// The first value of a header, matched without regard to case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Reads the whole body, refusing to hold more than `limit` bytes.
    pub fn body_bytes(&mut self, limit: usize) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        let read = (&mut *self).take(limit as u64 + 1).read_to_end(&mut out)?;
        if read > limit {
            return Err(Error::Protocol("response body is larger than the limit"));
        }
        Ok(out)
    }

    /// Reads the whole body as text, replacing invalid UTF-8.
    pub fn body_text(&mut self, limit: usize) -> Result<String, Error> {
        Ok(String::from_utf8_lossy(&self.body_bytes(limit)?).into_owned())
    }

    fn next_chunk(&mut self) -> io::Result<()> {
        let line = read_line(&mut self.stream).map_err(to_io)?;
        // Anything after ';' is a chunk extension, which nothing uses.
        let size_text = line.split(';').next().unwrap_or("").trim();
        if size_text.is_empty() || size_text.len() > 16 {
            return Err(invalid("bad chunk size"));
        }
        let size = u64::from_str_radix(size_text, 16).map_err(|_| invalid("bad chunk size"))?;
        if size == 0 {
            // Trailer headers, then a blank line.
            let mut count = 0;
            loop {
                let line = read_line(&mut self.stream).map_err(to_io)?;
                if line.is_empty() {
                    break;
                }
                count += 1;
                if count > MAX_HEADERS {
                    return Err(invalid("too many trailer headers"));
                }
            }
            self.framing = Framing::Done;
        } else {
            self.framing = Framing::ChunkData(size);
        }
        Ok(())
    }
}

/// Reads the response body.
impl<S: Read> Read for Response<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match self.framing {
                Framing::None | Framing::Done => return Ok(0),
                Framing::UntilClose => return self.stream.read(buf),
                Framing::Length(0) => {
                    self.framing = Framing::Done;
                }
                Framing::Length(remaining) => {
                    let want = buf
                        .len()
                        .min(usize::try_from(remaining).unwrap_or(usize::MAX));
                    let n = self.stream.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "connection closed before the whole body arrived",
                        ));
                    }
                    self.framing = Framing::Length(remaining - n as u64);
                    return Ok(n);
                }
                Framing::ChunkStart => self.next_chunk()?,
                Framing::ChunkData(remaining) => {
                    let want = buf
                        .len()
                        .min(usize::try_from(remaining).unwrap_or(usize::MAX));
                    let n = self.stream.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "connection closed in the middle of a chunk",
                        ));
                    }
                    if remaining == n as u64 {
                        // Each chunk's data ends with a line break.
                        let mut crlf = [0u8; 2];
                        self.stream.read_exact(&mut crlf)?;
                        if &crlf != b"\r\n" {
                            return Err(invalid("chunk is not followed by a line break"));
                        }
                        self.framing = Framing::ChunkStart;
                    } else {
                        self.framing = Framing::ChunkData(remaining - n as u64);
                    }
                    return Ok(n);
                }
            }
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn to_io(e: Error) -> io::Error {
    match e {
        Error::Io(e) => e,
        Error::Protocol(message) | Error::BadRequest(message) => invalid(message),
    }
}

/// Reads one line, without its line break. Accepts a bare `\n` as well as
/// `\r\n`.
fn read_line(stream: &mut impl BufRead) -> Result<String, Error> {
    let mut line = Vec::new();
    let read = stream
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)?;
    if read == 0 {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed before the response was complete",
        )));
    }
    if line.last() != Some(&b'\n') {
        return Err(if line.len() > MAX_LINE {
            Error::Protocol("line is too long")
        } else {
            Error::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed in the middle of a line",
            ))
        });
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| Error::Protocol("header is not valid UTF-8"))
}

fn read_status_line(stream: &mut impl BufRead) -> Result<(u16, String), Error> {
    let line = read_line(stream)?;
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(Error::Protocol("not an HTTP/1 response"));
    }
    let code = parts.next().unwrap_or("");
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Protocol("bad status code"));
    }
    let status: u16 = code
        .parse()
        .map_err(|_| Error::Protocol("bad status code"))?;
    Ok((status, parts.next().unwrap_or("").to_string()))
}

fn read_headers(stream: &mut impl BufRead) -> Result<Vec<(String, String)>, Error> {
    let mut headers = Vec::new();
    loop {
        let line = read_line(stream)?;
        if line.is_empty() {
            return Ok(headers);
        }
        if headers.len() >= MAX_HEADERS {
            return Err(Error::Protocol("too many headers"));
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // Folded headers are obsolete, and accepting them invites
            // disagreements about where a header ends.
            return Err(Error::Protocol("folded header line"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Error::Protocol("header line has no colon"));
        };
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(Error::Protocol("bad header name"));
        }
        headers.push((name.to_string(), value.trim().to_string()));
    }
}

fn body_framing(headers: &[(String, String)]) -> Result<Framing, Error> {
    fn values<'a>(headers: &'a [(String, String)], name: &'a str) -> impl Iterator<Item = &'a str> {
        headers
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    let mut chunked = false;
    for value in values(headers, "transfer-encoding") {
        for coding in value.split(',') {
            if coding.trim().eq_ignore_ascii_case("chunked") {
                chunked = true;
            } else if !coding.trim().is_empty() && !coding.trim().eq_ignore_ascii_case("identity") {
                return Err(Error::Protocol("unsupported transfer encoding"));
            }
        }
    }
    let mut length: Option<u64> = None;
    for value in values(headers, "content-length") {
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Protocol("bad Content-Length"));
        }
        let parsed: u64 = value
            .parse()
            .map_err(|_| Error::Protocol("bad Content-Length"))?;
        if length.is_some_and(|previous| previous != parsed) {
            return Err(Error::Protocol("conflicting Content-Length headers"));
        }
        length = Some(parsed);
    }
    Ok(if chunked {
        // Chunked wins over a length, as the standard requires.
        Framing::ChunkStart
    } else if let Some(length) = length {
        Framing::Length(length)
    } else {
        Framing::UntilClose
    })
}

/// Percent-encodes a string for use as one query parameter value or one
/// path segment.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A stream that replays a canned response and records what was sent.
    struct Mock<'a> {
        response: Cursor<Vec<u8>>,
        sent: &'a mut Vec<u8>,
        /// Hand out at most this many bytes per read, to exercise
        /// boundaries.
        step: usize,
    }

    impl Read for Mock<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.step);
            self.response.read(&mut buf[..n])
        }
    }

    impl Write for Mock<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.sent.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn mock<'a>(response: &[u8], sent: &'a mut Vec<u8>) -> Mock<'a> {
        Mock {
            response: Cursor::new(response.to_vec()),
            sent,
            step: usize::MAX,
        }
    }

    fn get(response: &[u8]) -> Result<Response<Mock<'static>>, Error> {
        let sent = Box::leak(Box::new(Vec::new()));
        Request::new("GET", "/", "h").send(mock(response, sent))
    }

    #[test]
    fn get_request_bytes() {
        let mut sent = Vec::new();
        Request::new("GET", "/v1/x?a=b", "docker")
            .header("Accept", "application/json")
            .send(mock(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                &mut sent,
            ))
            .unwrap();
        assert_eq!(
            String::from_utf8(sent).unwrap(),
            "GET /v1/x?a=b HTTP/1.1\r\nHost: docker\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
        );
    }

    #[test]
    fn post_bodies() {
        let ok = b"HTTP/1.1 204 No Content\r\n\r\n";

        let mut sent = Vec::new();
        Request::new("POST", "/start", "h")
            .send(mock(ok, &mut sent))
            .unwrap();
        assert!(String::from_utf8(sent)
            .unwrap()
            .ends_with("Content-Length: 0\r\n\r\n"));

        let mut sent = Vec::new();
        Request::new("POST", "/c", "h")
            .body(Body::Bytes(b"{\"a\":1}"))
            .send(mock(ok, &mut sent))
            .unwrap();
        assert!(String::from_utf8(sent)
            .unwrap()
            .ends_with("Content-Length: 7\r\n\r\n{\"a\":1}"));

        let mut sent = Vec::new();
        let mut data = &b"0123456789"[..];
        Request::new("PUT", "/c", "h")
            .body(Body::Sized(&mut data, 4))
            .send(mock(ok, &mut sent))
            .unwrap();
        assert!(String::from_utf8(sent)
            .unwrap()
            .ends_with("Content-Length: 4\r\n\r\n0123"));

        let mut sent = Vec::new();
        let mut data = &b"hello"[..];
        Request::new("PUT", "/c", "h")
            .body(Body::Chunked(&mut data))
            .send(mock(ok, &mut sent))
            .unwrap();
        assert!(String::from_utf8(sent)
            .unwrap()
            .ends_with("Transfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"));
    }

    #[test]
    fn short_sized_body_is_an_error() {
        let mut sent = Vec::new();
        let mut data = &b"ab"[..];
        let result = Request::new("PUT", "/c", "h")
            .body(Body::Sized(&mut data, 5))
            .send(mock(b"", &mut sent));
        assert!(matches!(result, Err(Error::BadRequest(_))));
    }

    #[test]
    fn rejects_header_injection() {
        let mut sent = Vec::new();
        for (name, value) in [
            ("X-A", "v\r\nEvil: 1"),
            ("X A", "v"),
            ("", "v"),
            ("Content-Length", "5"),
            ("host", "other"),
        ] {
            let result = Request::new("GET", "/", "h")
                .header(name, value)
                .send(mock(b"", &mut sent));
            assert!(
                matches!(result, Err(Error::BadRequest(_))),
                "{name}: {value}"
            );
        }
        for target in ["/a b", "/a\r\nX: 1", ""] {
            let result = Request::new("GET", target, "h").send(mock(b"", &mut sent));
            assert!(matches!(result, Err(Error::BadRequest(_))), "{target:?}");
        }
        let result = Request::new("GET", "/", "h\r\nX: 1").send(mock(b"", &mut sent));
        assert!(matches!(result, Err(Error::BadRequest(_))));
        assert!(
            sent.is_empty(),
            "nothing may be sent for a rejected request"
        );
    }

    #[test]
    fn content_length_body() {
        let mut r =
            get(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nX-Thing:  padded \r\n\r\nhelloEXTRA")
                .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.reason, "OK");
        assert!(r.is_success());
        assert_eq!(r.header("Content-Length"), Some("5"));
        assert_eq!(r.header("x-thing"), Some("padded"));
        assert_eq!(r.header("missing"), None);
        assert_eq!(r.body_bytes(100).unwrap(), b"hello");
    }

    #[test]
    fn chunked_body() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n6;ext=1\r\npedia \r\nE\r\nin \r\n\r\nchunks.\r\n0\r\nTrailer: x\r\n\r\n";
        assert_eq!(
            get(raw).unwrap().body_bytes(100).unwrap(),
            b"Wikipedia in \r\n\r\nchunks."
        );

        // The same, delivered one byte at a time.
        let mut sent = Vec::new();
        let mut stream = mock(raw, &mut sent);
        stream.step = 1;
        let mut r = Request::new("GET", "/", "h").send(stream).unwrap();
        assert_eq!(r.body_bytes(100).unwrap(), b"Wikipedia in \r\n\r\nchunks.");
    }

    #[test]
    fn body_until_close() {
        let mut r = get(b"HTTP/1.0 200 OK\r\n\r\nall of it").unwrap();
        assert_eq!(r.body_text(100).unwrap(), "all of it");
    }

    #[test]
    fn responses_without_bodies() {
        let mut r = get(b"HTTP/1.1 204 No Content\r\nContent-Length: 10\r\n\r\n").unwrap();
        assert_eq!(r.body_bytes(100).unwrap(), b"");
        let mut r = get(b"HTTP/1.1 304 Not Modified\r\n\r\nignored").unwrap();
        assert_eq!(r.body_bytes(100).unwrap(), b"");

        let mut sent = Vec::new();
        let mut r = Request::new("HEAD", "/", "h")
            .send(mock(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n",
                &mut sent,
            ))
            .unwrap();
        assert_eq!(r.body_bytes(100).unwrap(), b"");
    }

    #[test]
    fn informational_responses_are_skipped() {
        let mut r = get(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok",
        )
        .unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body_bytes(100).unwrap(), b"ok");
    }

    #[test]
    fn truncated_bodies_are_errors() {
        let mut r = get(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").unwrap();
        assert!(r.body_bytes(100).is_err());
        let mut r = get(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab").unwrap();
        assert!(r.body_bytes(100).is_err());
        let mut r = get(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabXX0\r\n\r\n")
            .unwrap();
        assert!(r.body_bytes(100).is_err());
        let mut r = get(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").unwrap();
        assert!(r.body_bytes(100).is_err());
    }

    #[test]
    fn body_limit() {
        let mut r = get(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nabcdef").unwrap();
        assert!(matches!(r.body_bytes(5), Err(Error::Protocol(_))));
        let mut r = get(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nabcdef").unwrap();
        assert_eq!(r.body_bytes(6).unwrap(), b"abcdef");
    }

    #[test]
    fn rejects_bad_heads() {
        for raw in [
            &b""[..],
            b"HTTP/1.1 200 OK\r\nContent-Length: 1",
            b"SSH-2.0-OpenSSH\r\n\r\n",
            b"HTTP/2 200 OK\r\n\r\n",
            b"HTTP/1.1 20 OK\r\n\r\n",
            b"HTTP/1.1 2000 OK\r\n\r\n",
            b"HTTP/1.1 abc OK\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nNoColon\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nBad Name: x\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nA: 1\r\n folded\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n",
        ] {
            assert!(get(raw).is_err(), "{:?}", String::from_utf8_lossy(raw));
        }
    }

    #[test]
    fn limits_on_head_size() {
        let long_line = format!(
            "HTTP/1.1 200 OK\r\nA: {}\r\n\r\n",
            "x".repeat(MAX_LINE + 10)
        );
        assert!(matches!(
            get(long_line.as_bytes()),
            Err(Error::Protocol("line is too long"))
        ));

        let mut many = String::from("HTTP/1.1 200 OK\r\n");
        for i in 0..=MAX_HEADERS {
            many.push_str(&format!("H{i}: v\r\n"));
        }
        many.push_str("\r\n");
        assert!(matches!(
            get(many.as_bytes()),
            Err(Error::Protocol("too many headers"))
        ));
    }

    #[test]
    fn chunked_wins_over_length() {
        let mut r = get(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n").unwrap();
        assert_eq!(r.body_bytes(100).unwrap(), b"ok");
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode("abc-_.~123"), "abc-_.~123");
        assert_eq!(percent_encode("a b/c:d?e&f=g"), "a%20b%2Fc%3Ad%3Fe%26f%3Dg");
        assert_eq!(percent_encode("é"), "%C3%A9");
        assert_eq!(percent_encode("C:\\job"), "C%3A%5Cjob");
    }
}
