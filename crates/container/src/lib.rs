// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! A client for the HTTP API that Docker and Podman both serve, over a
//! Unix socket on Linux and a named pipe on Windows.
//!
//! Only the calls the agent needs are here: pull an image, create a
//! container, copy files in and out, start it, follow its log, watch its
//! state, and remove it.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use ci_http::{percent_encode, Body, Request, Response};
use ci_json::Value;

/// The API version requested. Docker 29 accepts 1.40 and later, and
/// Podman's Docker-compatible API implements 1.41.
const API_PREFIX: &str = "/v1.41";

/// The `Host` header value. The runtimes ignore it, but HTTP/1.1 needs one.
const HOST: &str = "container-runtime";

/// Largest JSON reply held in memory.
const MAX_JSON_BODY: usize = 16 * 1024 * 1024;

/// Largest single log frame accepted from the runtime.
const MAX_LOG_FRAME: u32 = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum Error {
    /// The runtime could not be reached, or the connection broke.
    Io(io::Error),
    Http(ci_http::Error),
    /// The runtime answered with an error status.
    Api {
        status: u16,
        message: String,
    },
    /// The runtime's reply was not what its API documents.
    UnexpectedReply(String),
    /// A value given to the client cannot be used in a request.
    InvalidArgument(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "cannot talk to the container runtime: {e}"),
            Error::Http(e) => write!(f, "container runtime: {e}"),
            Error::Api { status, message } => {
                write!(f, "container runtime returned {status}: {message}")
            }
            Error::UnexpectedReply(message) => {
                write!(f, "unexpected reply from the container runtime: {message}")
            }
            Error::InvalidArgument(message) => write!(f, "invalid argument: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<ci_http::Error> for Error {
    fn from(e: ci_http::Error) -> Error {
        Error::Http(e)
    }
}

/// Where the runtime listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A Unix socket, such as `/var/run/docker.sock`.
    Unix(PathBuf),
    /// A Windows named pipe, such as `\\.\pipe\docker_engine`.
    NamedPipe(String),
}

impl Endpoint {
    /// Parses `unix:///path/to/socket` or `npipe:////./pipe/name`, the
    /// forms Docker uses for `DOCKER_HOST`.
    pub fn parse(text: &str) -> Result<Endpoint, Error> {
        if let Some(path) = text.strip_prefix("unix://") {
            if path.starts_with('/') {
                return Ok(Endpoint::Unix(PathBuf::from(path)));
            }
        } else if let Some(path) = text.strip_prefix("npipe://") {
            if path.starts_with("//./pipe/") && path.len() > "//./pipe/".len() {
                return Ok(Endpoint::NamedPipe(path.replace('/', "\\")));
            }
        }
        Err(Error::InvalidArgument(format!(
            "runtime endpoint must look like unix:///path or npipe:////./pipe/name, not {text:?}"
        )))
    }
}

/// One connection to the runtime.
pub enum Connection {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    #[cfg(windows)]
    Pipe(std::fs::File),
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Connection::Unix(s) => s.read(buf),
            #[cfg(windows)]
            Connection::Pipe(f) => match f.read(buf) {
                // ERROR_BROKEN_PIPE: the server closed its end, which is
                // how a named pipe reports end of stream.
                Err(e) if e.raw_os_error() == Some(109) => Ok(0),
                other => other,
            },
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Connection::Unix(s) => s.write(buf),
            #[cfg(windows)]
            Connection::Pipe(f) => f.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Connection::Unix(s) => s.flush(),
            #[cfg(windows)]
            Connection::Pipe(f) => f.flush(),
        }
    }
}

fn connect(endpoint: &Endpoint) -> Result<Connection, Error> {
    match endpoint {
        #[cfg(unix)]
        Endpoint::Unix(path) => Ok(Connection::Unix(std::os::unix::net::UnixStream::connect(
            path,
        )?)),
        #[cfg(not(unix))]
        Endpoint::Unix(_) => Err(Error::InvalidArgument(
            "Unix sockets are not supported on this platform".to_string(),
        )),
        #[cfg(windows)]
        Endpoint::NamedPipe(path) => {
            // A named pipe opens like a file. When every instance of the
            // pipe is in use the open fails with ERROR_PIPE_BUSY, and the
            // caller is expected to try again.
            let mut attempts = 0;
            loop {
                match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                {
                    Ok(file) => return Ok(Connection::Pipe(file)),
                    Err(e) if e.raw_os_error() == Some(231) && attempts < 100 => {
                        attempts += 1;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(e) => return Err(Error::Io(e)),
                }
            }
        }
        #[cfg(not(windows))]
        Endpoint::NamedPipe(_) => Err(Error::InvalidArgument(
            "named pipes are not supported on this platform".to_string(),
        )),
    }
}

/// What to run. The image should be named by digest, so that the same
/// description always runs the same bits.
#[derive(Debug, Clone, Default)]
pub struct ContainerSpec {
    pub image: String,
    pub command: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub working_dir: Option<String>,
    /// Windows only: `process` or `hyperv`.
    pub isolation: Option<String>,
    /// Labels to attach, so the containers can be found again later.
    pub labels: Vec<(String, String)>,
}

/// The state of a container, from an inspect call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerState {
    pub running: bool,
    /// Meaningful once `running` is false.
    pub exit_code: i64,
    /// Set when the runtime itself failed to start or run the container.
    pub error: String,
    pub oom_killed: bool,
    /// The id of the image the container was created from.
    pub image_id: String,
}

#[derive(Debug, Clone)]
pub struct Client {
    endpoint: Endpoint,
}

impl Client {
    pub fn new(endpoint: Endpoint) -> Client {
        Client { endpoint }
    }

    /// The runtime's version information. Also the cheapest way to check
    /// that the runtime is reachable.
    pub fn version(&self) -> Result<Value, Error> {
        self.json("GET", "/version", None)
    }

    /// Whether the image is already in the runtime's local store.
    pub fn image_exists(&self, image: &str) -> Result<bool, Error> {
        check_reference(image)?;
        let target = format!("{API_PREFIX}/images/{image}/json");
        let mut response = self.request("GET", &target, &[], Body::Empty)?;
        match response.status {
            200 => {
                drain(&mut response);
                Ok(true)
            }
            404 => {
                drain(&mut response);
                Ok(false)
            }
            _ => Err(api_error(&mut response)),
        }
    }

    /// Pulls an image. `progress` is called with each status line the
    /// runtime reports.
    pub fn pull_image(&self, image: &str, mut progress: impl FnMut(&str)) -> Result<(), Error> {
        check_reference(image)?;
        let target = format!(
            "{API_PREFIX}/images/create?fromImage={}",
            percent_encode(image)
        );
        let mut response = self.request("POST", &target, &[], Body::Empty)?;
        if !response.is_success() {
            return Err(api_error(&mut response));
        }
        // The body is a stream of JSON objects, one per line. A failure
        // part-way through arrives as an object with an "error" member,
        // under a 200 status.
        let mut lines = LineReader::new(&mut response);
        while let Some(line) = lines.next_line()? {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(event) = ci_json::parse(&line) else {
                return Err(Error::UnexpectedReply(format!(
                    "pull progress was not JSON: {line}"
                )));
            };
            if let Some(error) = event.get("error").and_then(Value::as_str) {
                return Err(Error::Api {
                    status: 500,
                    message: error.to_string(),
                });
            }
            if let Some(status) = event.get("status").and_then(Value::as_str) {
                progress(status);
            }
        }
        Ok(())
    }

    /// Creates a container without starting it, and returns its id.
    pub fn create_container(&self, spec: &ContainerSpec) -> Result<String, Error> {
        check_reference(&spec.image)?;
        let mut env = Vec::new();
        for (name, value) in &spec.environment {
            if name.is_empty() || name.contains('=') || name.contains('\0') || value.contains('\0')
            {
                return Err(Error::InvalidArgument(format!(
                    "bad environment variable name {name:?}"
                )));
            }
            env.push(format!("{name}={value}"));
        }
        let mut members = vec![
            ("Image", Value::from(spec.image.as_str())),
            ("Cmd", Value::strings(&spec.command)),
            ("Env", Value::strings(&env)),
            ("Tty", Value::Bool(false)),
            ("AttachStdin", Value::Bool(false)),
            ("OpenStdin", Value::Bool(false)),
        ];
        if let Some(dir) = &spec.working_dir {
            members.push(("WorkingDir", Value::from(dir.as_str())));
        }
        if !spec.labels.is_empty() {
            for (name, value) in &spec.labels {
                check_label(name)?;
                check_label(value)?;
            }
            members.push((
                "Labels",
                Value::object(
                    spec.labels
                        .iter()
                        .map(|(name, value)| (name.as_str(), Value::from(value.as_str()))),
                ),
            ));
        }
        if let Some(isolation) = &spec.isolation {
            members.push((
                "HostConfig",
                Value::object([("Isolation", Value::from(isolation.as_str()))]),
            ));
        }
        let body = Value::object(members).to_json();
        let reply = self.json("POST", "/containers/create", Some(&body))?;
        match reply.get("Id").and_then(Value::as_str) {
            Some(id) if is_container_id(id) => Ok(id.to_string()),
            _ => Err(Error::UnexpectedReply(
                "create returned no container id".to_string(),
            )),
        }
    }

    /// The ids of all containers, running or not, that carry a label
    /// with this name and value.
    pub fn containers_with_label(&self, name: &str, value: &str) -> Result<Vec<String>, Error> {
        check_label(name)?;
        check_label(value)?;
        let filters =
            Value::object([("label", Value::strings([format!("{name}={value}")]))]).to_json();
        let reply = self.json(
            "GET",
            &format!(
                "/containers/json?all=true&filters={}",
                percent_encode(&filters)
            ),
            None,
        )?;
        let list = reply.as_array().ok_or_else(|| {
            Error::UnexpectedReply("the container list is not an array".to_string())
        })?;
        let mut ids = Vec::new();
        for item in list {
            match item.get("Id").and_then(Value::as_str) {
                Some(id) if is_container_id(id) => ids.push(id.to_string()),
                _ => {
                    return Err(Error::UnexpectedReply(
                        "a listed container has no id".to_string(),
                    ))
                }
            }
        }
        Ok(ids)
    }

    /// Unpacks a tar archive into a directory of the container. The
    /// directory must already exist. Works on a container that has been
    /// created but not started.
    pub fn upload_archive(
        &self,
        id: &str,
        path: &str,
        archive: &mut dyn Read,
    ) -> Result<(), Error> {
        check_id(id)?;
        let target = format!(
            "{API_PREFIX}/containers/{id}/archive?path={}",
            percent_encode(path)
        );
        let mut response = self.request(
            "PUT",
            &target,
            &[("Content-Type", "application/x-tar")],
            Body::Chunked(archive),
        )?;
        if !response.is_success() {
            return Err(api_error(&mut response));
        }
        drain(&mut response);
        Ok(())
    }

    /// Fetches a file or directory of the container as a tar archive. The
    /// archive is read from the returned response.
    pub fn download_archive(&self, id: &str, path: &str) -> Result<Response<Connection>, Error> {
        check_id(id)?;
        let target = format!(
            "{API_PREFIX}/containers/{id}/archive?path={}",
            percent_encode(path)
        );
        let mut response = self.request("GET", &target, &[], Body::Empty)?;
        if !response.is_success() {
            return Err(api_error(&mut response));
        }
        Ok(response)
    }

    pub fn start_container(&self, id: &str) -> Result<(), Error> {
        check_id(id)?;
        let target = format!("{API_PREFIX}/containers/{id}/start");
        let mut response = self.request("POST", &target, &[], Body::Empty)?;
        // 304 means it was already started.
        if !response.is_success() && response.status != 304 {
            return Err(api_error(&mut response));
        }
        drain(&mut response);
        Ok(())
    }

    /// Follows the container's output from the beginning until the
    /// container stops.
    pub fn follow_logs(&self, id: &str) -> Result<LogStream, Error> {
        check_id(id)?;
        let target =
            format!("{API_PREFIX}/containers/{id}/logs?follow=true&stdout=true&stderr=true");
        let mut response = self.request("GET", &target, &[], Body::Empty)?;
        if !response.is_success() {
            return Err(api_error(&mut response));
        }
        Ok(LogStream { response })
    }

    pub fn inspect_container(&self, id: &str) -> Result<ContainerState, Error> {
        check_id(id)?;
        let reply = self.json("GET", &format!("/containers/{id}/json"), None)?;
        let state = reply
            .get("State")
            .ok_or_else(|| Error::UnexpectedReply("inspect returned no State".to_string()))?;
        let status = state.get("Status").and_then(Value::as_str).unwrap_or("");
        let running_flag = state
            .get("Running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(ContainerState {
            // "created" covers the moment between start being accepted and
            // the process existing; "restarting" and "paused" still hold a
            // live container.
            running: running_flag || matches!(status, "running" | "restarting" | "paused"),
            exit_code: state.get("ExitCode").and_then(Value::as_i64).unwrap_or(-1),
            error: state
                .get("Error")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            oom_killed: state
                .get("OOMKilled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            image_id: reply
                .get("Image")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }

    /// Kills the container's processes. A container that has already
    /// stopped is not an error.
    pub fn kill_container(&self, id: &str) -> Result<(), Error> {
        check_id(id)?;
        let target = format!("{API_PREFIX}/containers/{id}/kill");
        let mut response = self.request("POST", &target, &[], Body::Empty)?;
        if !response.is_success() && response.status != 409 && response.status != 404 {
            return Err(api_error(&mut response));
        }
        drain(&mut response);
        Ok(())
    }

    /// Removes the container and its anonymous volumes, killing it first
    /// if it is running. A container that is already gone is not an error.
    pub fn remove_container(&self, id: &str) -> Result<(), Error> {
        check_id(id)?;
        let target = format!("{API_PREFIX}/containers/{id}?force=true&v=true");
        let mut response = self.request("DELETE", &target, &[], Body::Empty)?;
        if !response.is_success() && response.status != 404 {
            return Err(api_error(&mut response));
        }
        drain(&mut response);
        Ok(())
    }

    fn request<'a>(
        &self,
        method: &'a str,
        target: &'a str,
        headers: &[(&'a str, &'a str)],
        body: Body<'a>,
    ) -> Result<Response<Connection>, Error> {
        let connection = connect(&self.endpoint)?;
        let mut request = Request::new(method, target, HOST).body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        Ok(request.send(connection)?)
    }

    fn json(&self, method: &str, path: &str, body: Option<&str>) -> Result<Value, Error> {
        let target = format!("{API_PREFIX}{path}");
        let mut response = match body {
            Some(body) => self.request(
                method,
                &target,
                &[("Content-Type", "application/json")],
                Body::Bytes(body.as_bytes()),
            )?,
            None => self.request(method, &target, &[], Body::Empty)?,
        };
        if !response.is_success() {
            return Err(api_error(&mut response));
        }
        let bytes = response.body_bytes(MAX_JSON_BODY)?;
        ci_json::parse_bytes(&bytes).map_err(|e| Error::UnexpectedReply(e.to_string()))
    }
}

/// Turns an error response into an [`Error::Api`]. The runtimes send
/// `{"message": "..."}`.
fn api_error(response: &mut Response<Connection>) -> Error {
    let status = response.status;
    let text = response.body_text(64 * 1024).unwrap_or_default();
    let message = ci_json::parse(&text)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| text.trim().to_string());
    Error::Api { status, message }
}

fn drain(response: &mut Response<Connection>) {
    let _ = io::copy(response, &mut io::sink());
}

fn is_container_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Container ids go into request paths, so only accept what a runtime
/// would actually return.
fn check_id(id: &str) -> Result<(), Error> {
    if is_container_id(id) {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!("bad container id {id:?}")))
    }
}

/// Label names and values go into a filter expression, where `=` and
/// quotes would change its meaning.
fn check_label(text: &str) -> Result<(), Error> {
    let ok = !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!("bad label {text:?}")))
    }
}

/// Image references go into request paths and query strings. This accepts
/// the characters a reference can contain and nothing else.
fn check_reference(image: &str) -> Result<(), Error> {
    let ok = !image.is_empty()
        && image.len() <= 512
        && !image.starts_with(['/', '-', '.'])
        && !image.contains("..")
        && !image.contains("//")
        && image
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/:@._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!(
            "bad image reference {image:?}"
        )))
    }
}

/// Which of the container's output streams a piece of log came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Stdout,
    Stderr,
}

/// A container's output, as the runtime sends it for a container without
/// a terminal: a sequence of frames, each with an 8-byte header giving
/// the stream and the length.
pub struct LogStream {
    response: Response<Connection>,
}

impl LogStream {
    /// Reads the next frame into `buf`, replacing its contents. Returns
    /// `None` when the container has stopped and all output has been read.
    pub fn next_frame(&mut self, buf: &mut Vec<u8>) -> Result<Option<Source>, Error> {
        read_frame(&mut self.response, buf)
    }
}

fn read_frame(input: &mut impl Read, buf: &mut Vec<u8>) -> Result<Option<Source>, Error> {
    let mut header = [0u8; 8];
    let mut filled = 0;
    while filled < header.len() {
        match input.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => {
                return Err(Error::UnexpectedReply(
                    "log stream ended inside a frame header".to_string(),
                ))
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
    let source = match header[0] {
        // Stream 0 is stdin, which some runtimes use for stdout.
        0 | 1 => Source::Stdout,
        2 => Source::Stderr,
        other => {
            return Err(Error::UnexpectedReply(format!(
                "log frame has unknown stream type {other}"
            )))
        }
    };
    let len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if len > MAX_LOG_FRAME {
        return Err(Error::UnexpectedReply(format!(
            "log frame of {len} bytes is too large"
        )));
    }
    buf.clear();
    buf.resize(len as usize, 0);
    input.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::UnexpectedReply("log stream ended inside a frame".to_string())
        } else {
            Error::Io(e)
        }
    })?;
    Ok(Some(source))
}

/// Splits a byte stream into lines without holding more than one line.
struct LineReader<'a, R: Read> {
    input: &'a mut R,
    buf: Vec<u8>,
    start: usize,
    eof: bool,
}

impl<'a, R: Read> LineReader<'a, R> {
    fn new(input: &'a mut R) -> LineReader<'a, R> {
        LineReader {
            input,
            buf: Vec::new(),
            start: 0,
            eof: false,
        }
    }

    fn next_line(&mut self) -> Result<Option<String>, Error> {
        loop {
            if let Some(pos) = self.buf[self.start..].iter().position(|&b| b == b'\n') {
                let line =
                    String::from_utf8_lossy(&self.buf[self.start..self.start + pos]).into_owned();
                self.start += pos + 1;
                return Ok(Some(line));
            }
            if self.eof {
                if self.start < self.buf.len() {
                    let line = String::from_utf8_lossy(&self.buf[self.start..]).into_owned();
                    self.start = self.buf.len();
                    return Ok(Some(line));
                }
                return Ok(None);
            }
            self.buf.drain(..self.start);
            self.start = 0;
            if self.buf.len() > MAX_JSON_BODY {
                return Err(Error::UnexpectedReply("line is too long".to_string()));
            }
            let mut chunk = [0u8; 8192];
            match self.input.read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parsing() {
        assert_eq!(
            Endpoint::parse("unix:///var/run/docker.sock").unwrap(),
            Endpoint::Unix(PathBuf::from("/var/run/docker.sock"))
        );
        assert_eq!(
            Endpoint::parse("npipe:////./pipe/docker_engine").unwrap(),
            Endpoint::NamedPipe("\\\\.\\pipe\\docker_engine".to_string())
        );
        for bad in [
            "",
            "/var/run/docker.sock",
            "unix://relative",
            "tcp://localhost:2375",
            "npipe://docker_engine",
            "npipe:////./pipe/",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn image_references() {
        for good in [
            "alpine",
            "alpine:3.20",
            "docker.io/library/alpine@sha256:0123abcd",
            "localhost:5000/a/b_c-d.e:tag",
            "mcr.microsoft.com/windows/nanoserver:ltsc2025",
        ] {
            assert!(check_reference(good).is_ok(), "{good}");
        }
        for bad in [
            "", "/alpine", "-flag", "a b", "a?b=c", "a#b", "a%2fb", "../etc", "a/../b", "a//b",
            "a\nb", "é",
        ] {
            assert!(check_reference(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn labels() {
        for good in ["ros2ci.agent", "0123abcd", "a-b_c.d"] {
            assert!(check_label(good).is_ok(), "{good}");
        }
        for bad in ["", "a=b", "a b", "a\"b", "a,b", "é"] {
            assert!(check_label(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn container_ids() {
        assert!(check_id("0123456789abcdef").is_ok());
        for bad in ["", "abc/def", "../x", "abc?x", "name-with-dash", "g123"] {
            assert!(check_id(bad).is_err(), "{bad:?}");
        }
        assert!(check_id(&"a".repeat(129)).is_err());
    }

    fn frame(kind: u8, data: &[u8]) -> Vec<u8> {
        let mut out = vec![kind, 0, 0, 0];
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn log_frames() {
        let mut raw = frame(1, b"out line\n");
        raw.extend(frame(2, b"err line\n"));
        raw.extend(frame(1, b""));
        raw.extend(frame(0, b"zero"));
        let mut input = &raw[..];
        let mut buf = Vec::new();
        assert_eq!(
            read_frame(&mut input, &mut buf).unwrap(),
            Some(Source::Stdout)
        );
        assert_eq!(buf, b"out line\n");
        assert_eq!(
            read_frame(&mut input, &mut buf).unwrap(),
            Some(Source::Stderr)
        );
        assert_eq!(buf, b"err line\n");
        assert_eq!(
            read_frame(&mut input, &mut buf).unwrap(),
            Some(Source::Stdout)
        );
        assert_eq!(buf, b"");
        assert_eq!(
            read_frame(&mut input, &mut buf).unwrap(),
            Some(Source::Stdout)
        );
        assert_eq!(buf, b"zero");
        assert_eq!(read_frame(&mut input, &mut buf).unwrap(), None);
    }

    #[test]
    fn bad_log_frames() {
        let mut buf = Vec::new();
        // Cut off inside the header.
        assert!(read_frame(&mut &[1u8, 0, 0][..], &mut buf).is_err());
        // Cut off inside the data.
        let mut raw = frame(1, b"full");
        raw.truncate(10);
        assert!(read_frame(&mut &raw[..], &mut buf).is_err());
        // Unknown stream type.
        assert!(read_frame(&mut &frame(9, b"x")[..], &mut buf).is_err());
        // A length that would be an enormous allocation.
        let huge = [1u8, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        assert!(read_frame(&mut &huge[..], &mut buf).is_err());
    }

    #[test]
    fn line_reader() {
        let mut input = &b"one\ntwo\n\nlast"[..];
        let mut lines = LineReader::new(&mut input);
        assert_eq!(lines.next_line().unwrap().as_deref(), Some("one"));
        assert_eq!(lines.next_line().unwrap().as_deref(), Some("two"));
        assert_eq!(lines.next_line().unwrap().as_deref(), Some(""));
        assert_eq!(lines.next_line().unwrap().as_deref(), Some("last"));
        assert_eq!(lines.next_line().unwrap(), None);
    }
}
