// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! URLs, TCP connections and TLS.
//!
//! TLS comes from rustls, the one third-party library the project allows.
//! This crate is the only place that touches it: everything else gets a
//! byte stream and does not know whether it is encrypted.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A URL could not be parsed or uses an unsupported scheme.
    Url(String),
    /// A certificate or key file could not be used.
    Tls(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Url(m) => write!(f, "invalid URL: {m}"),
            Error::Tls(m) => write!(f, "TLS setup: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

/// An `http://` or `https://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// The path and query, always starting with `/`.
    pub target: String,
}

impl Url {
    pub fn parse(text: &str) -> Result<Url, Error> {
        let bad = |what: &str| Error::Url(format!("{what}: {text:?}"));
        let (https, rest) = if let Some(rest) = text.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = text.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(bad("must start with http:// or https://"));
        };
        if text.bytes().any(|b| b <= b' ' || b == 0x7f) {
            return Err(bad("contains a space or control character"));
        }
        let rest = rest.split('#').next().unwrap_or("");
        let (authority, target) = match rest.find(['/', '?']) {
            Some(index) if rest.as_bytes()[index] == b'/' => {
                (&rest[..index], rest[index..].to_string())
            }
            Some(index) => (&rest[..index], format!("/{}", &rest[index..])),
            None => (rest, "/".to_string()),
        };
        if authority.contains('@') {
            // Credentials in URLs end up in logs.
            return Err(bad("must not contain a user name or password"));
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (host, after) = v6.split_once(']').ok_or_else(|| bad("unclosed '['"))?;
            match after.strip_prefix(':') {
                Some(port) => (host, Some(port)),
                None if after.is_empty() => (host, None),
                None => return Err(bad("unexpected text after ']'")),
            }
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(bad("has no host"));
        }
        let port = match port {
            Some(port) => port.parse().map_err(|_| bad("has a bad port"))?,
            None if https => 443,
            None => 80,
        };
        Ok(Url {
            https,
            host: host.to_string(),
            port,
            target,
        })
    }

    /// The value for the `Host` header.
    pub fn host_header(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let default_port = if self.https { 443 } else { 80 };
        if self.port == default_port {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }

    /// This URL with a different path and query.
    pub fn with_target(&self, target: &str) -> Url {
        Url {
            target: target.to_string(),
            ..self.clone()
        }
    }
}

/// A connection that may or may not be encrypted.
pub enum Stream {
    Plain(TcpStream),
    TlsClient(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    TlsServer(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let result = match self {
            Stream::Plain(s) => return s.read(buf),
            Stream::TlsClient(s) => s.read(buf),
            Stream::TlsServer(s) => s.read(buf),
        };
        match result {
            // A peer that closes the socket without the TLS goodbye. For
            // HTTP with a stated length the message is already complete,
            // and where it is not, the HTTP layer reports the truncation.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
            other => other,
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::TlsClient(s) => s.write(buf),
            Stream::TlsServer(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::TlsClient(s) => s.flush(),
            Stream::TlsServer(s) => s.flush(),
        }
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, Error> {
    let certificates: Vec<_> = CertificateDer::pem_file_iter(path)
        .map_err(|e| Error::Tls(format!("{}: {e}", path.display())))?
        .collect::<Result<_, _>>()
        .map_err(|e| Error::Tls(format!("{}: {e}", path.display())))?;
    if certificates.is_empty() {
        return Err(Error::Tls(format!(
            "{}: no certificates found",
            path.display()
        )));
    }
    Ok(certificates)
}

/// Makes outgoing connections.
#[derive(Clone)]
pub struct Connector {
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl Connector {
    /// A connector that can only make plain `http://` connections.
    pub fn plain_only() -> Connector {
        Connector { tls: None }
    }

    /// A connector that trusts the certificate authorities in a PEM file,
    /// and only those.
    pub fn with_ca_file(path: &Path) -> Result<Connector, Error> {
        let mut roots = rustls::RootCertStore::empty();
        for certificate in read_certificates(path)? {
            roots
                .add(certificate)
                .map_err(|e| Error::Tls(format!("{}: {e}", path.display())))?;
        }
        let config = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Tls(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Connector {
            tls: Some(Arc::new(config)),
        })
    }

    pub fn connect(&self, url: &Url) -> Result<Stream, Error> {
        let mut last_error = None;
        let mut socket = None;
        for address in (url.host.as_str(), url.port).to_socket_addrs()? {
            match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
                Ok(s) => {
                    socket = Some(s);
                    break;
                }
                Err(e) => last_error = Some(e),
            }
        }
        let socket = socket.ok_or_else(|| {
            Error::Io(last_error.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "the host name has no addresses")
            }))
        })?;
        socket.set_nodelay(true)?;
        socket.set_read_timeout(Some(IO_TIMEOUT))?;
        socket.set_write_timeout(Some(IO_TIMEOUT))?;
        if !url.https {
            return Ok(Stream::Plain(socket));
        }
        let config = self.tls.as_ref().ok_or_else(|| {
            Error::Tls("an https:// URL needs a certificate authority file".to_string())
        })?;
        let name = ServerName::try_from(url.host.clone())
            .map_err(|_| Error::Url(format!("{:?} is not a valid server name", url.host)))?;
        let connection = rustls::ClientConnection::new(Arc::clone(config), name)
            .map_err(|e| Error::Tls(e.to_string()))?;
        Ok(Stream::TlsClient(Box::new(rustls::StreamOwned::new(
            connection, socket,
        ))))
    }

    /// Connects and sends a request, returning the response to read.
    pub fn send(
        &self,
        url: &Url,
        request: ci_http::Request<'_>,
    ) -> Result<ci_http::Response<Stream>, RequestError> {
        let stream = self.connect(url).map_err(RequestError::Connect)?;
        request.send(stream).map_err(RequestError::Http)
    }
}

#[derive(Debug)]
pub enum RequestError {
    Connect(Error),
    Http(ci_http::Error),
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestError::Connect(e) => write!(f, "cannot connect: {e}"),
            RequestError::Http(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RequestError {}

/// Accepts incoming TLS connections with one certificate.
#[derive(Clone)]
pub struct Acceptor {
    config: Arc<rustls::ServerConfig>,
}

impl Acceptor {
    /// Loads a certificate chain and its private key from PEM files.
    pub fn from_pem_files(certificate: &Path, key: &Path) -> Result<Acceptor, Error> {
        let chain = read_certificates(certificate)?;
        let key = PrivateKeyDer::from_pem_file(key)
            .map_err(|e| Error::Tls(format!("{}: {e}", key.display())))?;
        let config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Tls(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| Error::Tls(e.to_string()))?;
        Ok(Acceptor {
            config: Arc::new(config),
        })
    }

    /// Wraps an accepted socket. The handshake happens on first use.
    pub fn accept(&self, socket: TcpStream) -> io::Result<Stream> {
        let connection = rustls::ServerConnection::new(Arc::clone(&self.config))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Stream::TlsServer(Box::new(rustls::StreamOwned::new(
            connection, socket,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let u = Url::parse("https://ci.example/worker/v1/claim?x=1").unwrap();
        assert_eq!(
            (u.https, u.host.as_str(), u.port, u.target.as_str()),
            (true, "ci.example", 443, "/worker/v1/claim?x=1")
        );
        assert_eq!(u.host_header(), "ci.example");

        let u = Url::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!(
            (u.https, u.host.as_str(), u.port, u.target.as_str()),
            (false, "127.0.0.1", 8080, "/")
        );
        assert_eq!(u.host_header(), "127.0.0.1:8080");

        let u = Url::parse("http://[::1]:9000/a#fragment").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.target.as_str()),
            ("::1", 9000, "/a")
        );
        assert_eq!(u.host_header(), "[::1]:9000");

        let u = Url::parse("http://host?query=1").unwrap();
        assert_eq!(u.target, "/?query=1");
        assert_eq!(u.with_target("/other").target, "/other");
    }

    #[test]
    fn bad_urls() {
        for bad in [
            "",
            "ftp://host/",
            "host/path",
            "http://",
            "http:///path",
            "http://host:port/",
            "http://host:99999/",
            "http://user:pw@host/",
            "http://host/a b",
            "http://host/a\r\nX: 1",
            "http://[::1/",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad:?}");
        }
    }
}
