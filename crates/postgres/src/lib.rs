// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! A PostgreSQL client: version 3 of the wire protocol, SCRAM-SHA-256
//! login, and parameterised queries with results in text format.
//!
//! One [`Connection`] is one session with the server and is used by one
//! thread at a time. Every query goes through the extended protocol, so
//! values are sent apart from the SQL text and are never spliced into it.

mod scram;

use std::fmt;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Largest message accepted from the server. A row with a larger value
/// than this is refused, not buffered.
const MAX_MESSAGE: usize = 256 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum Error {
    /// The server could not be reached, or the connection broke. The
    /// connection must not be used again.
    Io(io::Error),
    /// The server reported an error. The connection is still usable.
    Server(ServerError),
    /// The server sent something this client does not understand. The
    /// connection must not be used again.
    Protocol(String),
    /// Logging in failed for a reason other than the server's own error
    /// report, such as an unsupported authentication method.
    Auth(String),
    /// The connection settings or a query's arguments are wrong.
    Config(String),
}

/// An error reported by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerError {
    pub severity: String,
    /// The five-character SQLSTATE code, such as `23505` for a unique
    /// violation.
    pub code: String,
    pub message: String,
    pub detail: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "database connection error: {e}"),
            Error::Server(e) => {
                write!(f, "database error {}: {}", e.code, e.message)?;
                if !e.detail.is_empty() {
                    write!(f, " ({})", e.detail)?;
                }
                Ok(())
            }
            Error::Protocol(m) => write!(f, "database protocol error: {m}"),
            Error::Auth(m) => write!(f, "database login failed: {m}"),
            Error::Config(m) => write!(f, "database configuration error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl Error {
    /// The SQLSTATE code, when the server reported the error.
    pub fn sqlstate(&self) -> Option<&str> {
        match self {
            Error::Server(e) => Some(&e.code),
            _ => None,
        }
    }
}

/// Where the server is and who to log in as.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The password stays out of logs.
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Parses `postgres://user:password@host:port/database`. The port
    /// defaults to 5432 and the database to the user name. The user,
    /// password and database may be percent-encoded.
    pub fn parse(url: &str) -> Result<Config, Error> {
        let bad = |what: &str| Error::Config(format!("database URL: {what}"));
        let rest = url
            .strip_prefix("postgres://")
            .or_else(|| url.strip_prefix("postgresql://"))
            .ok_or_else(|| bad("must start with postgres://"))?;
        if rest.contains('?') {
            return Err(bad("options after '?' are not supported"));
        }
        let (authority, database) = match rest.split_once('/') {
            Some((authority, database)) => (authority, Some(database)),
            None => (rest, None),
        };
        // The password may contain '@', so split on the last one.
        let (credentials, address) = authority
            .rsplit_once('@')
            .ok_or_else(|| bad("needs a user name"))?;
        let (user, password) = match credentials.split_once(':') {
            Some((user, password)) => (user, password),
            None => (credentials, ""),
        };
        let user =
            percent_decode(user).ok_or_else(|| bad("bad percent-encoding in the user name"))?;
        let password =
            percent_decode(password).ok_or_else(|| bad("bad percent-encoding in the password"))?;
        if user.is_empty() {
            return Err(bad("needs a user name"));
        }
        let (host, port) = if let Some(v6) = address.strip_prefix('[') {
            let (host, after) = v6.split_once(']').ok_or_else(|| bad("unclosed '['"))?;
            match after.strip_prefix(':') {
                Some(port) => (host, Some(port)),
                None if after.is_empty() => (host, None),
                None => return Err(bad("unexpected text after ']'")),
            }
        } else {
            match address.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (address, None),
            }
        };
        if host.is_empty() {
            return Err(bad("needs a host"));
        }
        let port = match port {
            Some(port) => port.parse().map_err(|_| bad("bad port"))?,
            None => 5432,
        };
        let database = match database {
            Some(database) if !database.is_empty() => percent_decode(database)
                .ok_or_else(|| bad("bad percent-encoding in the database name"))?,
            _ => user.clone(),
        };
        for (name, value) in [("user name", &user), ("database name", &database)] {
            if value.contains('\0') {
                return Err(bad(&format!("the {name} contains a NUL")));
            }
        }
        Ok(Config {
            host: host.to_string(),
            port,
            user,
            password,
            database,
        })
    }
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A value sent with a query. Values travel in text format and the server
/// works out their types from the query, so `$1` in `WHERE id = $1` takes
/// the type of `id`. Where the server cannot tell, cast: `$1::bigint`.
#[derive(Debug, Clone, PartialEq)]
pub enum Param<'a> {
    Null,
    Text(&'a str),
    Int(i64),
    Bool(bool),
}

impl<'a> From<&'a str> for Param<'a> {
    fn from(s: &'a str) -> Param<'a> {
        Param::Text(s)
    }
}

impl<'a> From<&'a String> for Param<'a> {
    fn from(s: &'a String) -> Param<'a> {
        Param::Text(s)
    }
}

impl From<i64> for Param<'_> {
    fn from(n: i64) -> Self {
        Param::Int(n)
    }
}

impl From<bool> for Param<'_> {
    fn from(b: bool) -> Self {
        Param::Bool(b)
    }
}

impl<'a> From<Option<&'a str>> for Param<'a> {
    fn from(s: Option<&'a str>) -> Param<'a> {
        s.map_or(Param::Null, Param::Text)
    }
}

impl From<Option<i64>> for Param<'_> {
    fn from(n: Option<i64>) -> Self {
        n.map_or(Param::Null, Param::Int)
    }
}

/// The rows a query returned. Every value is the server's text form.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Row>,
    /// Rows changed by an INSERT, UPDATE or DELETE, or returned by a
    /// SELECT.
    pub affected: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row(Vec<Option<String>>);

impl Rows {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Row> {
        self.rows.iter()
    }

    /// The only row, or `None` when there are none.
    pub fn first(&self) -> Option<&Row> {
        self.rows.first()
    }
}

impl Row {
    /// The value in a column, or `None` for SQL NULL.
    ///
    /// # Panics
    ///
    /// Panics if the query did not return that many columns, which is a
    /// mistake in the calling code.
    pub fn get(&self, column: usize) -> Option<&str> {
        self.0[column].as_deref()
    }

    /// A text value that the schema says is never NULL.
    pub fn text(&self, column: usize) -> Result<&str, Error> {
        self.get(column)
            .ok_or_else(|| Error::Protocol(format!("column {column} is unexpectedly NULL")))
    }

    pub fn int(&self, column: usize) -> Result<i64, Error> {
        self.opt_int(column)?
            .ok_or_else(|| Error::Protocol(format!("column {column} is unexpectedly NULL")))
    }

    pub fn opt_int(&self, column: usize) -> Result<Option<i64>, Error> {
        match self.get(column) {
            None => Ok(None),
            Some(text) => text.parse().map(Some).map_err(|_| {
                Error::Protocol(format!("column {column} is not an integer: {text:?}"))
            }),
        }
    }

    pub fn bool(&self, column: usize) -> Result<bool, Error> {
        match self.text(column)? {
            "t" => Ok(true),
            "f" => Ok(false),
            other => Err(Error::Protocol(format!(
                "column {column} is not a boolean: {other:?}"
            ))),
        }
    }
}

pub struct Connection {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
    /// Set once the stream can no longer be trusted to be in step.
    broken: bool,
}

impl Connection {
    pub fn connect(config: &Config) -> Result<Connection, Error> {
        use std::net::ToSocketAddrs;
        let mut last_error = None;
        let mut stream = None;
        for address in (config.host.as_str(), config.port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last_error = Some(e),
            }
        }
        let stream = stream.ok_or_else(|| {
            Error::Io(last_error.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "the host name has no addresses")
            }))
        })?;
        stream.set_nodelay(true)?;
        let mut connection = Connection {
            reader: BufReader::new(stream.try_clone()?),
            writer: BufWriter::new(stream),
            broken: false,
        };
        connection.start_up(config)?;
        Ok(connection)
    }

    /// Runs one statement and returns its rows.
    pub fn query(&mut self, sql: &str, params: &[Param<'_>]) -> Result<Rows, Error> {
        self.check_usable()?;
        if sql.contains('\0') {
            return Err(Error::Config("the SQL text contains a NUL".to_string()));
        }
        if params.len() > i16::MAX as usize {
            return Err(Error::Config("too many parameters".to_string()));
        }
        // Checked before anything is written, so a bad value cannot leave
        // half a request in the output buffer.
        for param in params {
            if let Param::Text(text) = param {
                if text.contains('\0') {
                    return Err(Error::Config("a text parameter contains a NUL".to_string()));
                }
                if text.len() > MAX_MESSAGE {
                    return Err(Error::Config("a parameter is too large".to_string()));
                }
            }
        }
        let result = self.run_extended(sql, params);
        self.note_failure(&result);
        result
    }

    /// Runs one statement for its effect and returns the number of rows
    /// it changed.
    pub fn execute(&mut self, sql: &str, params: &[Param<'_>]) -> Result<u64, Error> {
        self.query(sql, params).map(|rows| rows.affected)
    }

    /// Runs several statements separated by semicolons, with no
    /// parameters. This is for schema migrations; anything that carries a
    /// value belongs in [`Connection::query`].
    pub fn batch(&mut self, sql: &str) -> Result<(), Error> {
        self.check_usable()?;
        if sql.contains('\0') {
            return Err(Error::Config("the SQL text contains a NUL".to_string()));
        }
        let result = self.run_simple(sql);
        self.note_failure(&result);
        result
    }

    /// Runs `body` inside a transaction, committing if it returns `Ok`
    /// and rolling back if it returns `Err`.
    pub fn transaction<T, E: From<Error>>(
        &mut self,
        body: impl FnOnce(&mut Connection) -> Result<T, E>,
    ) -> Result<T, E> {
        self.execute("BEGIN", &[])?;
        match body(self) {
            Ok(value) => {
                self.execute("COMMIT", &[])?;
                Ok(value)
            }
            Err(e) => {
                // If the connection is broken the server has already
                // rolled back, and this fails harmlessly.
                let _ = self.execute("ROLLBACK", &[]);
                Err(e)
            }
        }
    }

    /// Whether the connection can still be used. A broken one must be
    /// replaced.
    pub fn is_usable(&self) -> bool {
        !self.broken
    }

    fn check_usable(&self) -> Result<(), Error> {
        if self.broken {
            Err(Error::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "the database connection failed earlier and cannot be reused",
            )))
        } else {
            Ok(())
        }
    }

    fn note_failure<T>(&mut self, result: &Result<T, Error>) {
        // A server error leaves the stream in step. Anything else may
        // have stopped part-way through a message.
        if matches!(result, Err(Error::Io(_) | Error::Protocol(_))) {
            self.broken = true;
        }
    }

    fn start_up(&mut self, config: &Config) -> Result<(), Error> {
        let mut body = Vec::new();
        body.extend_from_slice(&196_608i32.to_be_bytes()); // protocol 3.0
        for (key, value) in [
            ("user", config.user.as_str()),
            ("database", config.database.as_str()),
            ("client_encoding", "UTF8"),
            ("application_name", "ros2ci"),
        ] {
            put_cstr(&mut body, key);
            put_cstr(&mut body, value);
        }
        body.push(0);
        // The start-up message alone has no type byte.
        self.writer
            .write_all(&((body.len() + 4) as i32).to_be_bytes())?;
        self.writer.write_all(&body)?;
        self.writer.flush()?;

        let mut pending_scram: Option<scram::Pending> = None;
        let mut scram_client: Option<scram::Client> = None;
        loop {
            let (kind, body) = self.read_message()?;
            let mut msg = Reader::new(&body);
            match kind {
                b'R' => match msg.i32()? {
                    0 => {
                        if scram_client.is_some() || pending_scram.is_some() {
                            return Err(Error::Auth(
                                "the server ended the password exchange early".to_string(),
                            ));
                        }
                    }
                    10 => {
                        let mut offered = Vec::new();
                        loop {
                            let name = msg.cstr()?;
                            if name.is_empty() {
                                break;
                            }
                            offered.push(name);
                        }
                        if !offered.iter().any(|m| m == "SCRAM-SHA-256") {
                            return Err(Error::Auth(format!(
                                "the server offers only {offered:?}, and this client needs SCRAM-SHA-256"
                            )));
                        }
                        let mut nonce = [0u8; 18];
                        ci_crypto::random_bytes(&mut nonce);
                        // PostgreSQL takes the user from the start-up
                        // message and ignores the one in the exchange.
                        let client =
                            scram::Client::new("", &config.password, &ci_crypto::base64_encode(&nonce));
                        let first = client.first_message();
                        let mut out = Vec::new();
                        put_cstr(&mut out, "SCRAM-SHA-256");
                        out.extend_from_slice(&(first.len() as i32).to_be_bytes());
                        out.extend_from_slice(first.as_bytes());
                        self.write_message(b'p', &out)?;
                        self.writer.flush()?;
                        scram_client = Some(client);
                    }
                    11 => {
                        let client = scram_client
                            .take()
                            .ok_or_else(|| Error::Protocol("unexpected SASL continue".to_string()))?;
                        let server_first = std::str::from_utf8(msg.rest())
                            .map_err(|_| Error::Protocol("SASL message is not UTF-8".to_string()))?;
                        let (reply, pending) = client
                            .final_message(server_first)
                            .map_err(|e| Error::Auth(e.to_string()))?;
                        self.write_message(b'p', reply.as_bytes())?;
                        self.writer.flush()?;
                        pending_scram = Some(pending);
                    }
                    12 => {
                        let pending = pending_scram
                            .take()
                            .ok_or_else(|| Error::Protocol("unexpected SASL final".to_string()))?;
                        let server_final = std::str::from_utf8(msg.rest())
                            .map_err(|_| Error::Protocol("SASL message is not UTF-8".to_string()))?;
                        pending.verify(server_final).map_err(|e| Error::Auth(e.to_string()))?;
                    }
                    3 => {
                        return Err(Error::Auth(
                            "the server asked for the password in clear text, which this client refuses to send"
                                .to_string(),
                        ))
                    }
                    5 => {
                        return Err(Error::Auth(
                            "the server asked for MD5 authentication, which this client does not support; \
                             set password_encryption to scram-sha-256"
                                .to_string(),
                        ))
                    }
                    other => {
                        return Err(Error::Auth(format!(
                            "the server asked for authentication method {other}, which is not supported"
                        )))
                    }
                },
                b'E' => return Err(Error::Server(parse_error(&body)?)),
                // Settings, the cancel key and notices: nothing to do.
                b'S' | b'K' | b'N' => {}
                b'Z' => return Ok(()),
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected message {:?} during start-up",
                        other as char
                    )))
                }
            }
        }
    }

    fn run_extended(&mut self, sql: &str, params: &[Param<'_>]) -> Result<Rows, Error> {
        // Parse: the unnamed statement, with parameter types left to the
        // server.
        let mut parse = Vec::with_capacity(sql.len() + 8);
        put_cstr(&mut parse, "");
        put_cstr(&mut parse, sql);
        parse.extend_from_slice(&0i16.to_be_bytes());
        self.write_message(b'P', &parse)?;

        // Bind: the unnamed portal, every parameter and result as text.
        let mut bind = Vec::new();
        put_cstr(&mut bind, "");
        put_cstr(&mut bind, "");
        bind.extend_from_slice(&0i16.to_be_bytes());
        bind.extend_from_slice(&(params.len() as i16).to_be_bytes());
        for param in params {
            let text = match param {
                Param::Null => {
                    bind.extend_from_slice(&(-1i32).to_be_bytes());
                    continue;
                }
                Param::Text(s) => std::borrow::Cow::Borrowed(*s),
                Param::Int(n) => std::borrow::Cow::Owned(n.to_string()),
                Param::Bool(b) => std::borrow::Cow::Borrowed(if *b { "t" } else { "f" }),
            };
            // Sizes were checked in `query`.
            let len = text.len() as i32;
            bind.extend_from_slice(&len.to_be_bytes());
            bind.extend_from_slice(text.as_bytes());
        }
        bind.extend_from_slice(&0i16.to_be_bytes());
        self.write_message(b'B', &bind)?;

        // Describe the portal, to learn the column names.
        self.write_message(b'D', b"P\0")?;
        // Execute, with no row limit.
        let mut execute = Vec::new();
        put_cstr(&mut execute, "");
        execute.extend_from_slice(&0i32.to_be_bytes());
        self.write_message(b'E', &execute)?;
        self.write_message(b'S', &[])?;
        self.writer.flush()?;

        self.read_results()
    }

    fn run_simple(&mut self, sql: &str) -> Result<(), Error> {
        let mut body = Vec::with_capacity(sql.len() + 1);
        put_cstr(&mut body, sql);
        self.write_message(b'Q', &body)?;
        self.writer.flush()?;
        self.read_results().map(|_| ())
    }

    /// Reads replies up to ReadyForQuery. A server error is remembered and
    /// returned once the stream is back in step.
    fn read_results(&mut self) -> Result<Rows, Error> {
        let mut rows = Rows::default();
        let mut server_error: Option<ServerError> = None;
        loop {
            let (kind, body) = self.read_message()?;
            let mut msg = Reader::new(&body);
            match kind {
                // ParseComplete, BindComplete, NoData, EmptyQueryResponse,
                // and settings or notices that can arrive at any time.
                b'1' | b'2' | b'n' | b'I' | b'S' | b'N' | b'A' => {}
                b'T' => {
                    let count = msg.i16()?;
                    rows.columns.clear();
                    for _ in 0..count {
                        rows.columns.push(msg.cstr()?);
                        // Table, column, type, size, modifier and format.
                        msg.skip(18)?;
                    }
                }
                b'D' => {
                    let count = msg.i16()?;
                    let mut row = Vec::with_capacity(count.max(0) as usize);
                    for _ in 0..count {
                        let len = msg.i32()?;
                        if len < 0 {
                            row.push(None);
                        } else {
                            let bytes = msg.bytes(len as usize)?;
                            row.push(Some(String::from_utf8(bytes.to_vec()).map_err(|_| {
                                Error::Protocol("a value is not valid UTF-8".to_string())
                            })?));
                        }
                    }
                    rows.rows.push(Row(row));
                }
                b'C' => {
                    // The tag ends with the row count: "INSERT 0 3",
                    // "UPDATE 2", "SELECT 5".
                    let tag = msg.cstr()?;
                    rows.affected = tag
                        .rsplit(' ')
                        .next()
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                }
                b'E' => {
                    if server_error.is_none() {
                        server_error = Some(parse_error(&body)?);
                    }
                }
                b'Z' => {
                    return match server_error {
                        Some(e) => Err(Error::Server(e)),
                        None => Ok(rows),
                    }
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected message {:?} in a query's reply",
                        other as char
                    )))
                }
            }
        }
    }

    fn write_message(&mut self, kind: u8, body: &[u8]) -> Result<(), Error> {
        let len = i32::try_from(body.len() + 4)
            .map_err(|_| Error::Config("the statement is too large".to_string()))?;
        self.writer.write_all(&[kind])?;
        self.writer.write_all(&len.to_be_bytes())?;
        self.writer.write_all(body)?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<(u8, Vec<u8>), Error> {
        let mut header = [0u8; 5];
        self.reader.read_exact(&mut header)?;
        let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        if len < 4 || len as usize - 4 > MAX_MESSAGE {
            return Err(Error::Protocol(format!(
                "message length {len} is out of range"
            )));
        }
        let mut body = vec![0u8; len as usize - 4];
        self.reader.read_exact(&mut body)?;
        Ok((header[0], body))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.broken {
            // Terminate: lets the server close the session at once.
            let _ = self.write_message(b'X', &[]);
            let _ = self.writer.flush();
        }
    }
}

fn put_cstr(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

fn parse_error(body: &[u8]) -> Result<ServerError, Error> {
    let mut msg = Reader::new(body);
    let mut error = ServerError {
        severity: String::new(),
        code: String::new(),
        message: String::new(),
        detail: String::new(),
    };
    loop {
        let field = msg.u8()?;
        if field == 0 {
            return Ok(error);
        }
        let value = msg.cstr()?;
        match field {
            b'S' => error.severity = value,
            b'C' => error.code = value,
            b'M' => error.message = value,
            b'D' => error.detail = value,
            _ => {}
        }
    }
}

/// Reads fields out of one message, failing cleanly if the message is
/// shorter than its contents claim.
struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data }
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], Error> {
        if len > self.data.len() {
            return Err(Error::Protocol(
                "message is shorter than its contents".to_string(),
            ));
        }
        let (head, tail) = self.data.split_at(len);
        self.data = tail;
        Ok(head)
    }

    fn skip(&mut self, len: usize) -> Result<(), Error> {
        self.bytes(len).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.bytes(1)?[0])
    }

    fn i16(&mut self) -> Result<i16, Error> {
        let b = self.bytes(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    fn i32(&mut self) -> Result<i32, Error> {
        let b = self.bytes(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn cstr(&mut self) -> Result<String, Error> {
        let end = self
            .data
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::Protocol("unterminated string in message".to_string()))?;
        let text = String::from_utf8(self.data[..end].to_vec())
            .map_err(|_| Error::Protocol("string in message is not UTF-8".to_string()))?;
        self.data = &self.data[end + 1..];
        Ok(text)
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let c = Config::parse("postgres://ci:secret@db.example:6432/builds").unwrap();
        assert_eq!(
            (
                c.host.as_str(),
                c.port,
                c.user.as_str(),
                c.password.as_str(),
                c.database.as_str()
            ),
            ("db.example", 6432, "ci", "secret", "builds")
        );

        let c = Config::parse("postgresql://ci@localhost").unwrap();
        assert_eq!(
            (c.port, c.password.as_str(), c.database.as_str()),
            (5432, "", "ci")
        );

        let c = Config::parse("postgres://u%40x:p%3A%2F%40ss@[::1]:5433/my%20db").unwrap();
        assert_eq!(c.user, "u@x");
        assert_eq!(c.password, "p:/@ss");
        assert_eq!(c.host, "::1");
        assert_eq!(c.port, 5433);
        assert_eq!(c.database, "my db");

        // An unencoded '@' in the password still works.
        let c = Config::parse("postgres://u:p@ss@host/db").unwrap();
        assert_eq!((c.password.as_str(), c.host.as_str()), ("p@ss", "host"));
    }

    #[test]
    fn bad_urls() {
        for bad in [
            "",
            "mysql://u@h/db",
            "postgres://host/db",
            "postgres://@host/db",
            "postgres://u@/db",
            "postgres://u@host:notaport/db",
            "postgres://u@host:99999/db",
            "postgres://u@[::1/db",
            "postgres://u@host/db?sslmode=require",
            "postgres://u%zz@host/db",
            "postgres://u%00@host/db",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn config_debug_hides_the_password() {
        let c = Config::parse("postgres://ci:hunter2@h/db").unwrap();
        assert!(!format!("{c:?}").contains("hunter2"));
    }

    #[test]
    fn error_message_fields() {
        let body = b"SERROR\0C23505\0Mduplicate key\0Dkey (id)=(1) exists\0Fignored\0\0";
        let e = parse_error(body).unwrap();
        assert_eq!(
            e,
            ServerError {
                severity: "ERROR".into(),
                code: "23505".into(),
                message: "duplicate key".into(),
                detail: "key (id)=(1) exists".into(),
            }
        );
        assert!(parse_error(b"SERROR\0Cunterminated").is_err());
        assert!(parse_error(b"").is_err());
    }

    #[test]
    fn reader_refuses_short_messages() {
        let mut r = Reader::new(&[0, 1, 2]);
        assert_eq!(r.i16().unwrap(), 1);
        assert!(r.i32().is_err());
        assert_eq!(r.u8().unwrap(), 2);
        assert!(r.u8().is_err());
        assert!(Reader::new(b"no terminator").cstr().is_err());
    }

    #[test]
    fn row_accessors() {
        let row = Row(vec![
            Some("42".into()),
            None,
            Some("t".into()),
            Some("x".into()),
        ]);
        assert_eq!(row.int(0).unwrap(), 42);
        assert_eq!(row.opt_int(1).unwrap(), None);
        assert!(row.int(1).is_err());
        assert!(row.bool(2).unwrap());
        assert!(row.bool(3).is_err());
        assert!(row.int(3).is_err());
        assert_eq!(row.text(3).unwrap(), "x");
        assert!(row.text(1).is_err());
    }
}
