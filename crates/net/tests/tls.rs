// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! A TLS server and client talking to each other on localhost, using the
//! test certificates in `fixtures/`.

use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;

use ci_http::server::{serve, Request, Response};
use ci_net::{Acceptor, Connector, RequestError, Url};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Starts an HTTPS server on a free port and returns the port.
fn start_server() -> u16 {
    let acceptor =
        Acceptor::from_pem_files(&fixture("server.pem"), &fixture("server.key")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        serve(
            listener,
            4,
            move |socket| acceptor.accept(socket),
            |request: &mut Request<'_>| {
                let body = request.body_bytes(8 << 20).unwrap().unwrap();
                Response::text(
                    200,
                    format!("{} {} {}", request.method, request.path, body.len()),
                )
            },
        )
    });
    port
}

#[test]
fn request_over_tls() {
    let port = start_server();
    let connector = Connector::with_ca_file(&fixture("ca.pem")).unwrap();
    for host in ["localhost", "127.0.0.1"] {
        let url = Url::parse(&format!("https://{host}:{port}/hello")).unwrap();
        let payload = vec![7u8; 3_000_000];
        let mut response = connector
            .send(
                &url,
                ci_http::Request::new("POST", &url.target, &url.host_header())
                    .body(ci_http::Body::Bytes(&payload)),
            )
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body_text(100).unwrap(), "POST /hello 3000000");
    }
}

#[test]
fn server_signed_by_an_unknown_authority_is_refused() {
    let port = start_server();
    let connector = Connector::with_ca_file(&fixture("other-ca.pem")).unwrap();
    let url = Url::parse(&format!("https://localhost:{port}/")).unwrap();
    let result = connector.send(&url, ci_http::Request::new("GET", "/", "localhost"));
    assert!(
        matches!(result, Err(RequestError::Http(_))),
        "the handshake must fail"
    );
}

#[test]
fn certificate_for_another_name_is_refused() {
    let port = start_server();
    let connector = Connector::with_ca_file(&fixture("ca.pem")).unwrap();
    // The certificate covers localhost and 127.0.0.1, not this name, even
    // though it reaches the same server.
    let url = Url::parse(&format!("https://127.0.0.2:{port}/")).unwrap();
    let result = connector.send(&url, ci_http::Request::new("GET", "/", "127.0.0.2"));
    assert!(result.is_err());
}

#[test]
fn https_needs_a_certificate_authority() {
    let port = start_server();
    let url = Url::parse(&format!("https://localhost:{port}/")).unwrap();
    let result = Connector::plain_only().send(&url, ci_http::Request::new("GET", "/", "localhost"));
    assert!(matches!(
        result,
        Err(RequestError::Connect(ci_net::Error::Tls(_)))
    ));
}

#[test]
fn plain_client_to_a_tls_server_fails_cleanly() {
    let port = start_server();
    let url = Url::parse(&format!("http://localhost:{port}/")).unwrap();
    let result = Connector::plain_only().send(&url, ci_http::Request::new("GET", "/", "localhost"));
    assert!(result.is_err());
}

#[test]
fn bad_certificate_files_are_reported() {
    assert!(Connector::with_ca_file(&fixture("missing.pem")).is_err());
    assert!(Connector::with_ca_file(&fixture("README.md")).is_err());
    assert!(Acceptor::from_pem_files(&fixture("server.pem"), &fixture("ca.pem")).is_err());
    assert!(Acceptor::from_pem_files(&fixture("server.key"), &fixture("server.key")).is_err());
}

#[test]
fn public_roots_do_not_trust_a_private_authority_unless_it_is_added() {
    let port = start_server();
    let url = Url::parse(&format!("https://localhost:{port}/hello")).unwrap();
    // The test server's certificate is signed by an authority no public
    // root vouches for.
    let public = Connector::with_public_roots(None).unwrap();
    assert!(public
        .send(&url, ci_http::Request::new("GET", "/hello", "localhost"))
        .is_err());
    // Adding that authority makes it trusted, alongside the public ones.
    let both = Connector::with_public_roots(Some(&fixture("ca.pem"))).unwrap();
    let response = both
        .send(&url, ci_http::Request::new("GET", "/hello", "localhost"))
        .unwrap();
    assert_eq!(response.status, 200);
    assert!(Connector::with_public_roots(Some(&fixture("missing.pem"))).is_err());
}
