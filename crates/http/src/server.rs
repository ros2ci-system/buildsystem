// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! An HTTP/1.1 server.
//!
//! It is deliberately small. Each connection carries one request and is
//! then closed, and a fixed pool of threads does blocking I/O. Anything
//! ambiguous in a request is refused outright, because ambiguity about
//! where a request ends is how request smuggling works.

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::{body_framing, is_token_byte, read_headers, read_line, BodyReader, Error, Framing};

/// How long a client may take to send its request or accept the reply
/// before the connection is dropped.
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Largest request target accepted.
const MAX_TARGET: usize = 8 * 1024;

pub struct Request<'a> {
    pub method: String,
    /// The path, percent-decoded, always starting with `/`.
    pub path: String,
    /// The query string as sent, without the `?`.
    pub query: String,
    headers: Vec<(String, String)>,
    /// The request body. Reading past its end returns end of file.
    pub body: &'a mut dyn Read,
    /// The body's length when the client stated it.
    pub content_length: Option<u64>,
}

impl Request<'_> {
    /// The first value of a header, matched without regard to case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The token from an `Authorization: Bearer` header.
    pub fn bearer_token(&self) -> Option<&str> {
        let value = self.header("authorization")?;
        let (scheme, token) = value.split_once(' ')?;
        if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
            Some(token.trim())
        } else {
            None
        }
    }

    /// Reads the whole body, refusing to hold more than `limit` bytes.
    /// Returns `None` when the body is larger.
    pub fn body_bytes(&mut self, limit: usize) -> io::Result<Option<Vec<u8>>> {
        if self.content_length.is_some_and(|len| len > limit as u64) {
            return Ok(None);
        }
        let mut out = Vec::new();
        let read = (&mut *self.body)
            .take(limit as u64 + 1)
            .read_to_end(&mut out)?;
        if read > limit {
            return Ok(None);
        }
        Ok(Some(out))
    }
}

pub enum ResponseBody {
    Bytes(Vec<u8>),
    /// A file of the given length, streamed from disk.
    File(File, u64),
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: ResponseBody,
}

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            content_type,
            body: ResponseBody::Bytes(body.into()),
        }
    }

    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Response {
        Response::new(status, "application/json", body)
    }

    pub fn text(status: u16, body: impl Into<Vec<u8>>) -> Response {
        Response::new(status, "text/plain; charset=utf-8", body)
    }

    pub fn empty(status: u16) -> Response {
        Response::new(status, "text/plain; charset=utf-8", Vec::new())
    }

    pub fn file(content_type: &'static str, file: File, len: u64) -> Response {
        Response {
            status: 200,
            content_type,
            body: ResponseBody::File(file, len),
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        414 => "URI Too Long",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// Serves one request on `stream`, then returns so the caller can close
/// it. Errors are I/O failures on the connection itself; a malformed
/// request is answered with an error status and is not an error here.
pub fn serve_connection<S: Read + Write>(
    stream: S,
    handler: &dyn Fn(&mut Request<'_>) -> Response,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    let response = match read_request_head(&mut reader) {
        Ok(head) => {
            let mut body = BodyReader {
                stream: &mut reader,
                framing: head.framing,
            };
            let mut request = Request {
                method: head.method,
                path: head.path,
                query: head.query,
                headers: head.headers,
                content_length: match head.framing {
                    Framing::Length(len) => Some(len),
                    Framing::None => Some(0),
                    _ => None,
                },
                body: &mut body,
            };
            handler(&mut request)
        }
        Err(Reject::Closed) => return Ok(()),
        Err(Reject::Io(e)) => return Err(e),
        Err(Reject::Status(status, message)) => Response::text(status, format!("{message}\n")),
    };
    // Whatever is left of the request body is not read: the connection is
    // closed after the reply, so the next request cannot be confused by it.
    write_response(reader.get_mut(), response)
}

struct Head {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    framing: Framing,
}

enum Reject {
    /// The client connected and sent nothing.
    Closed,
    Io(io::Error),
    Status(u16, &'static str),
}

fn read_request_head(reader: &mut impl io::BufRead) -> Result<Head, Reject> {
    let line = match read_line(reader) {
        Ok(line) => line,
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(Reject::Closed)
        }
        Err(Error::Io(e)) => return Err(Reject::Io(e)),
        Err(Error::Protocol("line is too long")) => {
            return Err(Reject::Status(414, "request line is too long"))
        }
        Err(_) => return Err(Reject::Status(400, "malformed request line")),
    };
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Reject::Status(400, "malformed request line"));
    };
    if version != "HTTP/1.1" {
        return Err(Reject::Status(505, "only HTTP/1.1 is supported"));
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(Reject::Status(400, "malformed method"));
    }
    if target.len() > MAX_TARGET {
        return Err(Reject::Status(414, "request target is too long"));
    }
    if !target.starts_with('/') {
        return Err(Reject::Status(400, "request target must be a path"));
    }
    let (raw_path, query) = target.split_once('?').unwrap_or((target, ""));
    let Some(path) = decode_path(raw_path) else {
        return Err(Reject::Status(400, "malformed path"));
    };

    let headers = match read_headers(reader) {
        Ok(headers) => headers,
        Err(Error::Io(e)) => return Err(Reject::Io(e)),
        Err(Error::Protocol("too many headers" | "line is too long")) => {
            return Err(Reject::Status(431, "request headers are too large"))
        }
        Err(_) => return Err(Reject::Status(400, "malformed headers")),
    };
    debug_assert!(headers.iter().all(|(n, _)| n.bytes().all(is_token_byte)));

    let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    if has("transfer-encoding") && has("content-length") {
        // The two disagree about where the body ends. Never guess.
        return Err(Reject::Status(
            400,
            "both Transfer-Encoding and Content-Length",
        ));
    }
    let framing = match body_framing(&headers) {
        // A request with neither header has no body. Only a response may
        // run until the connection closes.
        Ok(Framing::UntilClose) => Framing::None,
        Ok(framing) => framing,
        Err(Error::Protocol("unsupported transfer encoding")) => {
            return Err(Reject::Status(501, "unsupported transfer encoding"))
        }
        Err(_) => return Err(Reject::Status(400, "malformed body length")),
    };
    Ok(Head {
        method: method.to_string(),
        path,
        query: query.to_string(),
        headers,
        framing,
    })
}

/// Percent-decodes a path and refuses anything that could be mistaken
/// for a different path later: dot segments, empty segments, encoded
/// separators, control characters and invalid UTF-8.
fn decode_path(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            let decoded = u8::from_str_radix(hex, 16).ok()?;
            if decoded == b'/' || decoded == b'\\' {
                return None;
            }
            i += 3;
            decoded
        } else {
            i += 1;
            bytes[i - 1]
        };
        if byte < 0x20 || byte == 0x7f || byte == b'\\' {
            return None;
        }
        out.push(byte);
    }
    let path = String::from_utf8(out).ok()?;
    let mut segments = path.split('/');
    segments.next(); // before the leading slash
    let segments: Vec<&str> = segments.collect();
    for (index, segment) in segments.iter().enumerate() {
        let last = index == segments.len() - 1;
        if *segment == "." || *segment == ".." || (segment.is_empty() && !last) {
            return None;
        }
    }
    Some(path)
}

/// Splits a path into its segments: `/a/b` gives `["a", "b"]`.
pub fn path_segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

fn write_response(stream: &mut impl Write, response: Response) -> io::Result<()> {
    let len = match &response.body {
        ResponseBody::Bytes(bytes) => bytes.len() as u64,
        ResponseBody::File(_, len) => *len,
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        response.status,
        reason(response.status),
        len
    );
    if len > 0 {
        head.push_str("Content-Type: ");
        head.push_str(response.content_type);
        head.push_str("\r\n");
    }
    // Nothing served here should be interpreted or framed by a browser.
    head.push_str("X-Content-Type-Options: nosniff\r\n\r\n");
    match response.body {
        ResponseBody::Bytes(bytes) => {
            let mut out = head.into_bytes();
            out.extend_from_slice(&bytes);
            stream.write_all(&out)?;
        }
        ResponseBody::File(file, len) => {
            stream.write_all(head.as_bytes())?;
            let sent = io::copy(&mut file.take(len), stream)?;
            if sent != len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file was shorter than its stated length",
                ));
            }
        }
    }
    stream.flush()
}

/// Accepts connections forever, handling each on one of `threads` worker
/// threads. `wrap` turns an accepted socket into the stream to serve,
/// which is where TLS goes; for plain HTTP it returns the socket itself.
///
/// When every thread is busy and the queue is full, new connections are
/// closed at once. That keeps a flood from building an unbounded backlog.
pub fn serve<S, W, H>(listener: TcpListener, threads: usize, wrap: W, handler: H) -> io::Error
where
    S: Read + Write,
    W: Fn(TcpStream) -> io::Result<S> + Send + Sync + 'static,
    H: Fn(&mut Request<'_>) -> Response + Send + Sync + 'static,
{
    let (sender, receiver) = mpsc::sync_channel::<TcpStream>(threads * 4);
    let receiver = Arc::new(Mutex::new(receiver));
    let wrap = Arc::new(wrap);
    let handler = Arc::new(handler);
    for _ in 0..threads {
        let receiver = Arc::clone(&receiver);
        let wrap = Arc::clone(&wrap);
        let handler = Arc::clone(&handler);
        thread::spawn(move || loop {
            let next = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
            let Ok(socket) = next else { return };
            let _ = socket.set_read_timeout(Some(IO_TIMEOUT));
            let _ = socket.set_write_timeout(Some(IO_TIMEOUT));
            let _ = socket.set_nodelay(true);
            // A failure here concerns one client's connection. There is
            // nobody to report it to, and it must not stop the server.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(stream) = wrap(socket) {
                    let _ = serve_connection(stream, &*handler);
                }
            }));
            if result.is_err() {
                eprintln!("error: a request handler panicked");
            }
        });
    }
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                // Full queue: drop the connection, which closes it.
                let _ = sender.try_send(socket);
            }
            Err(e) if is_transient(&e) => thread::sleep(Duration::from_millis(50)),
            Err(e) => return e,
        }
    }
}

fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
    ) || e.raw_os_error() == Some(24) // too many open files: wait for some to close
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Mock {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Mock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Mock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Serves `raw` to an echo handler and returns the reply's status and
    /// body. The handler replies with what it understood of the request.
    fn exchange(raw: &[u8]) -> (u16, String) {
        let mut mock = Mock {
            input: Cursor::new(raw.to_vec()),
            output: Vec::new(),
        };
        serve_connection(&mut mock, &|request: &mut Request<'_>| {
            let body = request.body_bytes(1000).unwrap();
            match body {
                Some(body) => Response::text(
                    200,
                    format!(
                        "{} {} ?{} token={:?} body={}",
                        request.method,
                        request.path,
                        request.query,
                        request.bearer_token(),
                        String::from_utf8_lossy(&body)
                    ),
                ),
                None => Response::empty(413),
            }
        })
        .unwrap();
        let text = String::from_utf8(mock.output).unwrap();
        if text.is_empty() {
            return (0, String::new());
        }
        let status: u16 = text[9..12].parse().unwrap();
        let body = text.split_once("\r\n\r\n").unwrap().1.to_string();
        (status, body)
    }

    #[test]
    fn simple_requests() {
        assert_eq!(
            exchange(b"GET /a/b?x=1&y=2 HTTP/1.1\r\nHost: h\r\n\r\n"),
            (200, "GET /a/b ?x=1&y=2 token=None body=".to_string())
        );
        assert_eq!(
            exchange(b"POST /claim HTTP/1.1\r\nHost: h\r\nAuthorization: Bearer abc123\r\nContent-Length: 5\r\n\r\nhelloEXTRA"),
            (200, "POST /claim ? token=Some(\"abc123\") body=hello".to_string())
        );
        assert_eq!(
            exchange(b"PUT /up HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n"),
            (200, "PUT /up ? token=None body=abcde".to_string())
        );
        // Bare line feeds are tolerated.
        assert_eq!(exchange(b"GET / HTTP/1.1\nHost: h\n\n").0, 200);
    }

    #[test]
    fn response_head() {
        let mut mock = Mock {
            input: Cursor::new(b"GET / HTTP/1.1\r\n\r\n".to_vec()),
            output: Vec::new(),
        };
        serve_connection(&mut mock, &|_: &mut Request<'_>| Response::json(201, "{}")).unwrap();
        assert_eq!(
            String::from_utf8(mock.output).unwrap(),
            "HTTP/1.1 201 Created\r\nConnection: close\r\nContent-Length: 2\r\n\
             Content-Type: application/json\r\nX-Content-Type-Options: nosniff\r\n\r\n{}"
        );
    }

    #[test]
    fn percent_decoding_of_paths() {
        assert_eq!(
            exchange(b"GET /a%20b/c%2Bd HTTP/1.1\r\n\r\n").1,
            "GET /a b/c+d ? token=None body="
        );
        assert_eq!(
            exchange(b"GET /dir/ HTTP/1.1\r\n\r\n").1,
            "GET /dir/ ? token=None body="
        );
    }

    #[test]
    fn unsafe_paths_are_refused() {
        for target in [
            "/../etc/passwd",
            "/a/../b",
            "/a/./b",
            "/a//b",
            "/a/%2e%2e/b",
            "/a%2fb",
            "/a%5cb",
            "/a\\b",
            "/a%00b",
            "/a%0ab",
            "/a%zzb",
            "/a%2",
            "/%ff",
            "relative",
            "http://host/absolute",
            "*",
        ] {
            let raw = format!("GET {target} HTTP/1.1\r\n\r\n");
            assert_eq!(exchange(raw.as_bytes()).0, 400, "{target}");
        }
    }

    #[test]
    fn malformed_requests_are_refused() {
        for (raw, status) in [
            (&b"GET /\r\n\r\n"[..], 400),
            (b"GET / HTTP/1.1 extra\r\n\r\n", 400),
            (b"get / HTTP/1.1\r\n\r\n", 400),
            (b" / HTTP/1.1\r\n\r\n", 400),
            (b"GET / HTTP/1.0\r\n\r\n", 505),
            (b"GET / HTTP/2\r\n\r\n", 505),
            (b"GET / HTTP/1.1\r\nNo colon here\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nBad Name: x\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nA: 1\r\n folded\r\n\r\n", 400),
            (b"POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n", 400),
            (b"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab", 400),
            (b"POST / HTTP/1.1\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n", 400),
            (b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n", 501),
        ] {
            assert_eq!(exchange(raw).0, status, "{:?}", String::from_utf8_lossy(raw));
        }
    }

    #[test]
    fn size_limits() {
        let long_target = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_TARGET + 1));
        assert_eq!(exchange(long_target.as_bytes()).0, 414);
        let huge_line = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(100_000));
        assert_eq!(exchange(huge_line.as_bytes()).0, 414);
        let mut many = String::from("GET / HTTP/1.1\r\n");
        for i in 0..300 {
            many.push_str(&format!("H{i}: v\r\n"));
        }
        many.push_str("\r\n");
        assert_eq!(exchange(many.as_bytes()).0, 431);
        // The echo handler accepts bodies up to 1000 bytes.
        let big = format!(
            "POST / HTTP/1.1\r\nContent-Length: 1001\r\n\r\n{}",
            "x".repeat(1001)
        );
        assert_eq!(exchange(big.as_bytes()).0, 413);
        let chunked = format!(
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3e9\r\n{}\r\n0\r\n\r\n",
            "x".repeat(1001)
        );
        assert_eq!(exchange(chunked.as_bytes()).0, 413);
    }

    #[test]
    fn silent_client_gets_no_reply() {
        assert_eq!(exchange(b"").0, 0);
    }

    #[test]
    fn bearer_tokens() {
        let token = |header: &str| {
            let raw = format!("GET / HTTP/1.1\r\n{header}\r\n\r\n");
            exchange(raw.as_bytes()).1
        };
        assert!(token("Authorization: Bearer tok").contains("Some(\"tok\")"));
        assert!(token("authorization: bearer tok").contains("Some(\"tok\")"));
        assert!(token("Authorization: Basic dXNlcjpwYXNz").contains("None"));
        assert!(token("Authorization: Bearer").contains("None"));
        assert!(token("X-Other: Bearer tok").contains("None"));
    }

    #[test]
    fn path_segment_splitting() {
        assert_eq!(path_segments("/a/b"), ["a", "b"]);
        assert_eq!(path_segments("/a/b/"), ["a", "b"]);
        assert!(path_segments("/").is_empty());
    }

    #[test]
    fn serves_over_tcp_with_the_client() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            serve(listener, 4, Ok, |request: &mut Request<'_>| {
                let body = request.body_bytes(1 << 20).unwrap().unwrap();
                Response::text(
                    200,
                    format!("{} {} {}", request.method, request.path, body.len()),
                )
            })
        });
        let handles: Vec<_> = (0..16)
            .map(|i| {
                thread::spawn(move || {
                    let stream = TcpStream::connect(address).unwrap();
                    let payload = vec![b'x'; i * 1000];
                    let mut response = crate::Request::new("POST", "/echo", "test")
                        .body(crate::Body::Bytes(&payload))
                        .send(stream)
                        .unwrap();
                    assert_eq!(response.status, 200);
                    assert_eq!(
                        response.body_text(1000).unwrap(),
                        format!("POST /echo {}", i * 1000)
                    );
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    }
}
