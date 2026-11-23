// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Builds real images through a real container runtime.
//!
//! These tests run only when `CI_CONTAINER_RUNTIME` names a runtime. They
//! use a small Linux base image on a Linux host and a small Windows one
//! on a Windows host, and remove the images they build.

use std::time::{Duration, Instant};

use ci_container::{BuildEvent, BuildOptions, Client, Endpoint, Error};

#[cfg(not(windows))]
mod platform {
    pub const BASE: &str = "docker.io/library/alpine:3.20";
    pub const RUN_OK: &str = "RUN echo building-step && cat /hello.txt";
    pub const RUN_FAIL: &str = "RUN echo before-failure && exit 7";
    pub const RUN_SLOW: &str = "RUN echo slow-step-started && sleep 300";
    pub const COPY: &str = "COPY hello.txt /hello.txt";
}

#[cfg(windows)]
mod platform {
    pub const BASE: &str = "mcr.microsoft.com/windows/nanoserver:ltsc2025";
    pub const RUN_OK: &str = "RUN echo building-step && type C:\\hello.txt";
    pub const RUN_FAIL: &str = "RUN echo before-failure && exit 7";
    pub const RUN_SLOW: &str = "RUN echo slow-step-started && ping -n 300 127.0.0.1 >NUL";
    pub const COPY: &str = "COPY hello.txt C:/hello.txt";
}

fn client() -> Option<Client> {
    match std::env::var("CI_CONTAINER_RUNTIME") {
        Ok(value) if !value.is_empty() => Some(Client::new(Endpoint::parse(&value).unwrap())),
        _ => {
            eprintln!("skipped: CI_CONTAINER_RUNTIME is not set");
            None
        }
    }
}

fn options(client: &Client, tag: &str, build_file: &str) -> BuildOptions {
    let windows = client
        .version()
        .unwrap()
        .get("Os")
        .and_then(|v| v.as_str())
        .is_some_and(|os| os.eq_ignore_ascii_case("windows"));
    BuildOptions {
        tag: tag.to_string(),
        build_file: build_file.to_string(),
        isolation: windows.then(|| "process".to_string()),
        labels: vec![("ros2ci.test".to_string(), "build".to_string())],
    }
}

fn context(build_file_path: &str, body: &str) -> Vec<u8> {
    let mut builder = ci_tar::Builder::new(Vec::new());
    builder
        .append_file(build_file_path, 0o644, body.as_bytes())
        .unwrap();
    builder
        .append_file("hello.txt", 0o644, b"hello from the context\n")
        .unwrap();
    builder.finish().unwrap()
}

/// Runs a build to its end and returns its log and its failure, if any.
fn run_build(client: &Client, options: &BuildOptions, context: &[u8]) -> (String, Option<String>) {
    let (mut stream, _canceller) = client.build_image(options, &mut &context[..]).unwrap();
    let mut log = String::new();
    let mut failure = None;
    while let Some(event) = stream.next_event().unwrap() {
        match event {
            BuildEvent::Output(text) => log.push_str(&text),
            BuildEvent::Failed(message) => failure = Some(message),
        }
    }
    (log, failure)
}

#[test]
fn builds_an_image_from_a_context() {
    let Some(client) = client() else { return };
    let tag = format!("ros2ci-test/build-ok:{}", std::process::id());
    let build_file = format!(
        "FROM {}\n{}\n{}\n",
        platform::BASE,
        platform::COPY,
        platform::RUN_OK
    );
    // The build file sits in a subdirectory of the context.
    let options = options(&client, &tag, "images/test/Dockerfile");
    assert_eq!(client.image_id(&tag).unwrap(), None);

    let (log, failure) = run_build(
        &client,
        &options,
        &context("images/test/Dockerfile", &build_file),
    );
    assert_eq!(failure, None, "{log}");
    assert!(log.contains("building-step"), "{log}");
    assert!(log.contains("hello from the context"), "{log}");

    let id = client
        .image_id(&tag)
        .unwrap()
        .expect("the image should exist under its tag");
    assert!(id.starts_with("sha256:"), "{id}");
    assert!(client.image_exists(&tag).unwrap());

    // Building the same thing again gives the same image.
    let (_, failure) = run_build(
        &client,
        &options,
        &context("images/test/Dockerfile", &build_file),
    );
    assert_eq!(failure, None);
    assert_eq!(client.image_id(&tag).unwrap().as_deref(), Some(id.as_str()));

    client.remove_image(&tag).unwrap();
    assert_eq!(client.image_id(&tag).unwrap(), None);
    client.remove_image(&tag).unwrap();
}

#[test]
fn a_failing_step_fails_the_build() {
    let Some(client) = client() else { return };
    let tag = format!("ros2ci-test/build-fail:{}", std::process::id());
    let build_file = format!("FROM {}\n{}\n", platform::BASE, platform::RUN_FAIL);
    let (log, failure) = run_build(
        &client,
        &options(&client, &tag, "Dockerfile"),
        &context("Dockerfile", &build_file),
    );
    assert!(failure.is_some(), "the build should have failed:\n{log}");
    assert!(log.contains("before-failure"), "{log}");
    assert_eq!(
        client.image_id(&tag).unwrap(),
        None,
        "a failed build must not leave an image"
    );
}

#[test]
fn a_missing_build_file_is_reported() {
    let Some(client) = client() else { return };
    let tag = format!("ros2ci-test/build-missing:{}", std::process::id());
    let build_file = format!("FROM {}\n", platform::BASE);
    let options = options(&client, &tag, "no/such/Dockerfile");
    let outcome = client
        .build_image(&options, &mut &context("Dockerfile", &build_file)[..])
        .map(|(mut stream, _)| {
            let mut failure = None;
            while let Ok(Some(event)) = stream.next_event() {
                if let BuildEvent::Failed(message) = event {
                    failure = Some(message);
                }
            }
            failure
        });
    match outcome {
        Err(Error::Api { .. }) | Ok(Some(_)) => {}
        other => panic!("expected the build to be refused, got {other:?}"),
    }
    assert_eq!(client.image_id(&tag).unwrap(), None);
}

#[test]
fn bad_build_arguments_are_refused_before_any_request() {
    let Some(client) = client() else { return };
    let build_file = format!("FROM {}\n", platform::BASE);
    for path in [
        "",
        "/abs/Dockerfile",
        "../Dockerfile",
        "a/../b",
        "a//b",
        "a\\b",
    ] {
        let options = options(&client, "ros2ci-test/x:1", path);
        let result = client.build_image(&options, &mut &context("Dockerfile", &build_file)[..]);
        assert!(matches!(result, Err(Error::InvalidArgument(_))), "{path:?}");
    }
    let options = options(&client, "bad tag", "Dockerfile");
    assert!(matches!(
        client.build_image(&options, &mut &context("Dockerfile", &build_file)[..]),
        Err(Error::InvalidArgument(_))
    ));
}

/// On Linux a build can be stopped by closing its connection. On Windows
/// it cannot, so there this test only checks that asking is harmless.
#[test]
fn a_running_build_can_be_cancelled() {
    let Some(client) = client() else { return };
    if cfg!(windows) {
        eprintln!("skipped: a build cannot be cancelled over a named pipe");
        return;
    }
    let tag = format!("ros2ci-test/build-cancel:{}", std::process::id());
    let build_file = format!("FROM {}\n{}\n", platform::BASE, platform::RUN_SLOW);
    let options = options(&client, &tag, "Dockerfile");
    let (mut stream, canceller) = client
        .build_image(&options, &mut &context("Dockerfile", &build_file)[..])
        .unwrap();
    let started = Instant::now();
    let mut cancelled = false;
    // The stream ends, or fails, once the build has stopped.
    while let Ok(Some(BuildEvent::Output(text))) = stream.next_event() {
        if !cancelled && text.contains("slow-step-started") {
            assert!(canceller.cancel());
            cancelled = true;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "the build did not stop"
        );
    }
    assert!(cancelled, "the slow step never started");
    assert!(started.elapsed() < Duration::from_secs(120));
    assert_eq!(client.image_id(&tag).unwrap(), None);
}
