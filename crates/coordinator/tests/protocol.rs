// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Runs the real coordinator against a real PostgreSQL server and talks
//! to it as a worker would, over HTTP. No containers are involved: the
//! "worker" here is the test itself, which lets it misbehave on purpose.
//!
//! These tests run only when `CI_POSTGRES_URL` names a server whose user
//! may create databases. Each test gets its own database and its own
//! coordinator process, and drops the database afterwards.

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ci_crypto::{base64_encode, hex_encode, sha256};
use ci_http::{Body, Request};
use ci_json::Value;
use ci_net::{Connector, Url};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct Coordinator {
    process: Option<Child>,
    worker_url: String,
    admin_url: String,
    database: String,
    database_url: String,
    data_dir: PathBuf,
    options: Vec<String>,
    worker_port: u16,
    admin_port: u16,
}

fn admin_database_url() -> Option<String> {
    match std::env::var("CI_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ => {
            eprintln!("skipped: CI_POSTGRES_URL is not set");
            None
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Coordinator {
    /// Starts a coordinator on a fresh database. `options` are extra
    /// command-line options, such as a short lease.
    fn start(options: &[&str]) -> Option<Coordinator> {
        let admin_url = admin_database_url()?;
        let config = ci_postgres::Config::parse(&admin_url).unwrap();
        let database = format!(
            "ros2ci_test_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        let mut admin = ci_postgres::Connection::connect(&config).unwrap();
        admin
            .batch(&format!("DROP DATABASE IF EXISTS {database}"))
            .unwrap();
        admin.batch(&format!("CREATE DATABASE {database}")).unwrap();
        let database_url = format!("{}/{database}", admin_url.rsplit_once('/').unwrap().0);

        let data_dir = std::env::temp_dir().join(&database);
        let _ = fs::remove_dir_all(&data_dir);
        let mut coordinator = Coordinator {
            process: None,
            worker_url: String::new(),
            admin_url: String::new(),
            database,
            database_url,
            data_dir,
            options: options.iter().map(|s| s.to_string()).collect(),
            worker_port: free_port(),
            admin_port: free_port(),
        };
        coordinator.worker_url = format!("http://127.0.0.1:{}", coordinator.worker_port);
        coordinator.admin_url = format!("http://127.0.0.1:{}", coordinator.admin_port);
        coordinator.process = Some(coordinator.spawn());
        coordinator.wait_until_listening();
        Some(coordinator)
    }

    fn spawn(&self) -> Child {
        Command::new(env!("CARGO_BIN_EXE_ci-coordinator"))
            .env("CI_DATABASE_URL", &self.database_url)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--worker-listen")
            .arg(format!("127.0.0.1:{}", self.worker_port))
            .arg("--admin-listen")
            .arg(format!("127.0.0.1:{}", self.admin_port))
            .args(&self.options)
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn wait_until_listening(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", self.worker_port)).is_ok()
                && TcpStream::connect(("127.0.0.1", self.admin_port)).is_ok()
            {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("the coordinator did not start listening");
    }

    fn stop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }

    fn restart(&mut self) {
        self.stop();
        self.process = Some(self.spawn());
        self.wait_until_listening();
    }

    /// The first admin token, which the coordinator wrote to its data
    /// directory when it started.
    fn admin_token(&self) -> String {
        fs::read_to_string(self.data_dir.join("admin-token"))
            .unwrap()
            .trim()
            .to_string()
    }

    /// Sends a request and returns the status and the reply's bytes. A
    /// call to the admin API with no token given uses the admin token.
    fn call(
        &self,
        base: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&[u8]>,
    ) -> (u16, Vec<u8>) {
        let admin_token;
        let token = match token {
            None if base == self.admin_url => {
                admin_token = self.admin_token();
                Some(admin_token.as_str())
            }
            other => other,
        };
        self.raw(base, method, path, token, body)
    }

    /// Sends a request with exactly the token given, or none.
    fn raw(
        &self,
        base: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&[u8]>,
    ) -> (u16, Vec<u8>) {
        let url = Url::parse(&format!("{base}{path}")).unwrap();
        let host = url.host_header();
        let authorization = token.map(|t| format!("Bearer {t}"));
        let mut request = Request::new(method, &url.target, &host);
        if let Some(value) = &authorization {
            request = request.header("Authorization", value);
        }
        if let Some(body) = body {
            request = request.body(Body::Bytes(body));
        }
        let mut response = Connector::plain_only().send(&url, request).unwrap();
        let status = response.status;
        (status, response.body_bytes(64 << 20).unwrap())
    }

    fn json(
        &self,
        base: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let text = body.map(|b| b.to_json());
        let (status, bytes) = self.call(
            base,
            method,
            path,
            token,
            text.as_deref().map(str::as_bytes),
        );
        let value = ci_json::parse_bytes(&bytes).unwrap_or_else(|e| {
            panic!("{method} {path} returned {status} with a non-JSON body: {e}")
        });
        (status, value)
    }

    fn admin(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        self.json(&self.admin_url, method, path, None, body)
    }

    fn worker(&self, path: &str, token: &str, body: Value) -> (u16, Value) {
        self.json(&self.worker_url, "POST", path, Some(token), Some(body))
    }

    /// Creates an enrolment token for a pool and registers a worker with
    /// it, returning the worker's session token.
    fn register(&self, pool: &str, os: &str, arch: &str) -> String {
        let (status, reply) = self.admin(
            "POST",
            "/admin/v1/worker-tokens",
            Some(Value::object([
                ("name", Value::from("test worker")),
                ("pool", Value::from(pool)),
            ])),
        );
        assert_eq!(status, 201, "{reply:?}");
        let enrolment = reply.get("token").unwrap().as_str().unwrap().to_string();
        let (status, reply) = self.worker(
            "/worker/v1/register",
            &enrolment,
            Value::object([
                ("protocol_version", Value::Int(1)),
                ("os", Value::from(os)),
                ("arch", Value::from(arch)),
            ]),
        );
        assert_eq!(status, 200, "{reply:?}");
        assert_eq!(reply.get("pool").unwrap().as_str(), Some(pool));
        reply
            .get("session_token")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    fn submit(&self, pool: &str, os: &str, arch: &str) -> i64 {
        let (status, reply) =
            self.admin("POST", "/admin/v1/jobs", Some(job_request(pool, os, arch)));
        assert_eq!(status, 201, "{reply:?}");
        reply.get("job_id").unwrap().as_i64().unwrap()
    }

    /// Claims a job, returning its id and attempt number.
    fn claim(&self, session: &str) -> Option<(i64, i64)> {
        let (status, reply) = self.worker("/worker/v1/claim", session, Value::object::<&str>([]));
        assert_eq!(status, 200, "{reply:?}");
        let job = reply.get("job").unwrap();
        if job.is_null() {
            return None;
        }
        assert_eq!(
            job.get("description")
                .unwrap()
                .get("image")
                .unwrap()
                .as_str(),
            Some("registry.example/image@sha256:0123")
        );
        Some((
            job.get("job_id").unwrap().as_i64().unwrap(),
            job.get("attempt").unwrap().as_i64().unwrap(),
        ))
    }

    /// Claims, waiting out a retry delay if there is one.
    fn claim_eventually(&self, session: &str) -> (i64, i64) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(job) = self.claim(session) {
                return job;
            }
            assert!(Instant::now() < deadline, "no job became claimable");
            thread::sleep(Duration::from_millis(200));
        }
    }

    fn report(&self, session: &str, job: (i64, i64), offset: i64, chunk: &[u8]) -> (String, i64) {
        let (status, reply) = self.worker(
            "/worker/v1/report",
            session,
            Value::object([
                ("job_id", Value::Int(job.0)),
                ("attempt", Value::Int(job.1)),
                ("log_offset", Value::Int(offset)),
                ("log_base64", Value::from(base64_encode(chunk))),
            ]),
        );
        assert_eq!(status, 200, "{reply:?}");
        (
            reply.get("action").unwrap().as_str().unwrap().to_string(),
            reply.get("log_offset").unwrap().as_i64().unwrap(),
        )
    }

    fn finish(
        &self,
        session: &str,
        job: (i64, i64),
        outcome: &str,
        artifacts: Vec<Value>,
    ) -> (u16, Value) {
        self.worker(
            "/worker/v1/finish",
            session,
            Value::object([
                ("job_id", Value::Int(job.0)),
                ("attempt", Value::Int(job.1)),
                ("outcome", Value::from(outcome)),
                (
                    "exit_code",
                    if outcome == "success" {
                        Value::Int(0)
                    } else {
                        Value::Null
                    },
                ),
                ("message", Value::from(format!("test says {outcome}"))),
                ("image_id", Value::from("sha256:abcd")),
                ("artifacts", Value::Array(artifacts)),
            ]),
        )
    }

    fn status(&self, job_id: i64) -> Value {
        let (status, reply) = self.admin("GET", &format!("/admin/v1/jobs/{job_id}"), None);
        assert_eq!(status, 200, "{reply:?}");
        reply
    }

    fn wait_for_state(&self, job_id: i64, state: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = self.status(job_id);
            if status.get("state").unwrap().as_str() == Some(state) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "job {job_id} never became {state}: {status:?}"
            );
            thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.data_dir);
        if let Some(url) = admin_database_url() {
            if let Ok(mut admin) =
                ci_postgres::Connection::connect(&ci_postgres::Config::parse(&url).unwrap())
            {
                let _ = admin.batch(&format!(
                    "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                    self.database
                ));
            }
        }
    }
}

fn description() -> Value {
    Value::object([
        ("image", Value::from("registry.example/image@sha256:0123")),
        ("command", Value::strings(["run"])),
        ("timeout_seconds", Value::Int(60)),
    ])
}

fn job_request(pool: &str, os: &str, arch: &str) -> Value {
    Value::object([
        ("pool", Value::from(pool)),
        ("os", Value::from(os)),
        ("arch", Value::from(arch)),
        ("description", description()),
    ])
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).unwrap().as_str().unwrap()
}

#[test]
fn a_job_from_submission_to_finish() {
    let Some(c) = Coordinator::start(&[]) else {
        return;
    };
    let session = c.register("release", "linux", "amd64");
    assert_eq!(c.claim(&session), None, "nothing is queued yet");

    let job_id = c.submit("release", "linux", "amd64");
    assert_eq!(str_of(&c.status(job_id), "state"), "queued");
    let job = c.claim(&session).unwrap();
    assert_eq!(job, (job_id, 1));
    assert_eq!(str_of(&c.status(job_id), "state"), "running");
    assert_eq!(
        c.claim(&session),
        None,
        "a running job is not handed out twice"
    );

    // The log arrives in pieces, with one piece sent twice and one sent
    // too early, as happens when replies are lost.
    assert_eq!(
        c.report(&session, job, 0, b"first line\n"),
        ("continue".to_string(), 11)
    );
    assert_eq!(
        c.report(&session, job, 0, b"first line\n"),
        ("continue".to_string(), 11)
    );
    assert_eq!(
        c.report(&session, job, 50, b"from the future"),
        ("continue".to_string(), 11)
    );
    assert_eq!(
        c.report(&session, job, 11, b"second \xff line\n"),
        ("continue".to_string(), 25)
    );
    assert_eq!(
        c.report(&session, job, 20, b"line\nthird\n"),
        ("continue".to_string(), 31)
    );

    let artifact = b"{\"tests\": []}".to_vec();
    let path = format!("/worker/v1/jobs/{job_id}/attempts/1/artifacts/out/results.json");
    let (status, reply) = c.json(&c.worker_url, "PUT", &path, Some(&session), None);
    assert_eq!(status, 200, "an empty upload is allowed: {reply:?}");
    let (status, bytes) = c.call(&c.worker_url, "PUT", &path, Some(&session), Some(&artifact));
    assert_eq!(status, 200);
    let reply = ci_json::parse_bytes(&bytes).unwrap();
    assert_eq!(reply.get("size").unwrap().as_i64(), Some(13));
    assert_eq!(str_of(&reply, "sha256"), hex_encode(&sha256(&artifact)));

    let manifest = vec![Value::object([
        ("path", Value::from("out/results.json")),
        ("size", Value::Int(13)),
        ("sha256", Value::from(hex_encode(&sha256(&artifact)))),
    ])];
    let (status, reply) = c.finish(&session, job, "success", manifest.clone());
    assert_eq!(status, 200, "{reply:?}");
    // A Finish whose reply was lost is sent again, and is harmless.
    let (status, _) = c.finish(&session, job, "success", manifest);
    assert_eq!(status, 200);

    let done = c.status(job_id);
    assert_eq!(str_of(&done, "state"), "finished");
    assert_eq!(str_of(&done, "outcome"), "success");
    assert_eq!(done.get("exit_code").unwrap().as_i64(), Some(0));
    assert_eq!(str_of(&done, "image_id"), "sha256:abcd");
    assert_eq!(done.get("attempt").unwrap().as_i64(), Some(1));
    assert!(str_of(&done, "finished_at").ends_with('Z'));
    let listed = done.get("artifacts").unwrap().as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(str_of(&listed[0], "path"), "out/results.json");

    let (status, log) = c.call(
        &c.admin_url,
        "GET",
        &format!("/admin/v1/jobs/{job_id}/log"),
        None,
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(log, b"first line\nsecond \xff line\nthird\n");
    let (status, fetched) = c.call(
        &c.admin_url,
        "GET",
        &format!("/admin/v1/jobs/{job_id}/artifacts/out/results.json"),
        None,
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(fetched, artifact);

    // Once finished, the job takes no more reports.
    assert_eq!(c.report(&session, job, 31, b"late").0, "abandon");
}

#[test]
fn calls_without_a_valid_token_are_refused() {
    let Some(c) = Coordinator::start(&[]) else {
        return;
    };
    let session = c.register("release", "linux", "amd64");
    let empty = || Some(Value::object::<&str>([]));
    for path in ["/worker/v1/claim", "/worker/v1/report", "/worker/v1/finish"] {
        assert_eq!(
            c.json(&c.worker_url, "POST", path, None, empty()).0,
            401,
            "{path} with no token"
        );
        assert_eq!(
            c.json(&c.worker_url, "POST", path, Some("not-a-token"), empty())
                .0,
            401,
            "{path} with a made-up token"
        );
    }
    let upload = "/worker/v1/jobs/1/attempts/1/artifacts/x";
    assert_eq!(
        c.call(&c.worker_url, "PUT", upload, None, Some(b"x")).0,
        401
    );

    let register = |token: Option<&str>| {
        c.json(
            &c.worker_url,
            "POST",
            "/worker/v1/register",
            token,
            Some(Value::object([
                ("protocol_version", Value::Int(1)),
                ("os", Value::from("linux")),
                ("arch", Value::from("amd64")),
            ])),
        )
        .0
    };
    assert_eq!(register(None), 401);
    assert_eq!(register(Some("not-a-token")), 401);
    // A session token is not an enrolment token.
    assert_eq!(register(Some(&session)), 401);

    // The admin API is not reachable through the worker listener, with or
    // without a worker's token.
    for token in [None, Some(session.as_str())] {
        assert_eq!(
            c.json(
                &c.worker_url,
                "POST",
                "/admin/v1/jobs",
                token,
                Some(job_request("release", "linux", "amd64"))
            )
            .0,
            404
        );
        assert_eq!(
            c.json(
                &c.worker_url,
                "POST",
                "/admin/v1/worker-tokens",
                token,
                empty()
            )
            .0,
            404
        );
        assert_eq!(
            c.json(&c.worker_url, "GET", "/admin/v1/jobs/1", token, None)
                .0,
            404
        );
    }
    // And the worker API is not on the admin listener: a worker's token
    // is not an admin token, and with an admin token the path is unknown.
    let claim = |token: &str| {
        c.raw(
            &c.admin_url,
            "POST",
            "/worker/v1/claim",
            Some(token),
            Some(b"{}"),
        )
        .0
    };
    assert_eq!(claim(&session), 401);
    assert_eq!(claim(&c.admin_token()), 404);
}

#[test]
fn a_worker_only_gets_jobs_for_its_pool_and_platform() {
    let Some(c) = Coordinator::start(&[]) else {
        return;
    };
    let release = c.register("release", "linux", "amd64");
    let pull_request = c.register("pull-request", "linux", "amd64");
    let windows = c.register("release", "windows", "amd64");
    let arm = c.register("release", "linux", "arm64");

    let job_id = c.submit("release", "linux", "amd64");
    assert_eq!(c.claim(&pull_request), None, "wrong pool");
    assert_eq!(c.claim(&windows), None, "wrong operating system");
    assert_eq!(c.claim(&arm), None, "wrong architecture");
    assert_eq!(c.claim(&release), Some((job_id, 1)));

    // One worker cannot act on another worker's job.
    assert_eq!(
        c.report(&pull_request, (job_id, 1), 0, b"intruder").0,
        "abandon"
    );
    assert_eq!(
        c.finish(&pull_request, (job_id, 1), "success", vec![]).0,
        409
    );
    let upload = format!("/worker/v1/jobs/{job_id}/attempts/1/artifacts/x");
    assert_eq!(
        c.call(
            &c.worker_url,
            "PUT",
            &upload,
            Some(&pull_request),
            Some(b"x")
        )
        .0,
        409
    );
    assert_eq!(str_of(&c.status(job_id), "state"), "running");
    assert_eq!(c.status(job_id).get("log_bytes").unwrap().as_i64(), Some(0));
}

#[test]
fn jobs_are_handed_out_oldest_first_and_only_once() {
    let Some(c) = Coordinator::start(&[]) else {
        return;
    };
    let sessions: Vec<String> = (0..4)
        .map(|_| c.register("release", "linux", "amd64"))
        .collect();
    let submitted: Vec<i64> = (0..12)
        .map(|_| c.submit("release", "linux", "amd64"))
        .collect();

    // Four workers claim at once until the queue is empty.
    let claimed: Vec<i64> = thread::scope(|scope| {
        let handles: Vec<_> = sessions
            .iter()
            .map(|session| {
                let c = &c;
                scope.spawn(move || {
                    let mut mine = Vec::new();
                    while let Some((job_id, attempt)) = c.claim(session) {
                        assert_eq!(attempt, 1);
                        mine.push(job_id);
                    }
                    mine
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    let mut sorted = claimed.clone();
    sorted.sort();
    assert_eq!(sorted, submitted, "every job claimed exactly once");

    let c2 = Coordinator::start(&[]).unwrap();
    let session = c2.register("release", "linux", "amd64");
    let ids: Vec<i64> = (0..3)
        .map(|_| c2.submit("release", "linux", "amd64"))
        .collect();
    let order: Vec<i64> = (0..3).map(|_| c2.claim(&session).unwrap().0).collect();
    assert_eq!(order, ids, "oldest first");
}

#[test]
fn a_silent_worker_loses_its_job() {
    let Some(c) = Coordinator::start(&[
        "--lease-seconds",
        "2",
        "--report-interval-seconds",
        "1",
        "--retry-backoff-seconds",
        "1",
        "--max-infrastructure-failures",
        "2",
    ]) else {
        return;
    };
    let lost = c.register("release", "linux", "amd64");
    let healthy = c.register("release", "linux", "amd64");
    let job_id = c.submit("release", "linux", "amd64");

    let first = c.claim(&lost).unwrap();
    assert_eq!(first, (job_id, 1));
    assert_eq!(c.report(&lost, first, 0, b"attempt one\n").0, "continue");
    // The worker goes quiet. The lease runs out and the job is requeued.
    let status = c.wait_for_state(job_id, "queued");
    assert_eq!(
        status.get("infrastructure_failures").unwrap().as_i64(),
        Some(1)
    );

    let second = c.claim_eventually(&healthy);
    assert_eq!(second, (job_id, 2), "the same job, as a new attempt");

    // The first worker comes back. Nothing it sends is accepted.
    assert_eq!(c.report(&lost, first, 12, b"too late\n").0, "abandon");
    assert_eq!(c.finish(&lost, first, "success", vec![]).0, 409);
    let upload = format!("/worker/v1/jobs/{job_id}/attempts/1/artifacts/x");
    assert_eq!(
        c.call(&c.worker_url, "PUT", &upload, Some(&lost), Some(b"x"))
            .0,
        409
    );
    // Not even with the new attempt's number, which it could guess.
    assert_eq!(c.report(&lost, second, 0, b"guess").0, "abandon");

    // The second attempt's log starts from nothing.
    assert_eq!(
        c.report(&healthy, second, 0, b"attempt two\n"),
        ("continue".to_string(), 12)
    );
    let (_, log) = c.call(
        &c.admin_url,
        "GET",
        &format!("/admin/v1/jobs/{job_id}/log"),
        None,
        None,
    );
    assert_eq!(log, b"attempt two\n");

    // The second worker goes quiet too. That is the limit: the job ends.
    let done = c.wait_for_state(job_id, "finished");
    assert_eq!(str_of(&done, "outcome"), "infrastructure_failure");
    assert_eq!(
        done.get("infrastructure_failures").unwrap().as_i64(),
        Some(2)
    );
    assert_eq!(c.claim(&healthy), None);
}

#[test]
fn reporting_keeps_the_lease_alive() {
    let Some(c) = Coordinator::start(&["--lease-seconds", "2", "--report-interval-seconds", "1"])
    else {
        return;
    };
    let session = c.register("release", "linux", "amd64");
    let job_id = c.submit("release", "linux", "amd64");
    let job = c.claim(&session).unwrap();
    // Three times the lease, with a report every half second.
    let end = Instant::now() + Duration::from_secs(6);
    while Instant::now() < end {
        assert_eq!(c.report(&session, job, 0, b"").0, "continue");
        thread::sleep(Duration::from_millis(500));
    }
    assert_eq!(str_of(&c.status(job_id), "state"), "running");
    assert_eq!(c.finish(&session, job, "success", vec![]).0, 200);
}

#[test]
fn an_infrastructure_failure_is_retried_with_a_delay() {
    let Some(c) = Coordinator::start(&[
        "--retry-backoff-seconds",
        "2",
        "--max-infrastructure-failures",
        "3",
    ]) else {
        return;
    };
    let session = c.register("release", "linux", "amd64");
    let job_id = c.submit("release", "linux", "amd64");

    let first = c.claim(&session).unwrap();
    assert_eq!(
        c.finish(&session, first, "infrastructure_failure", vec![])
            .0,
        200
    );
    let status = c.status(job_id);
    assert_eq!(str_of(&status, "state"), "queued");
    assert_eq!(
        str_of(&status, "message"),
        "test says infrastructure_failure"
    );
    assert_eq!(c.claim(&session), None, "the retry waits out its delay");

    let started = Instant::now();
    let second = c.claim_eventually(&session);
    assert_eq!(second, (job_id, 2));
    assert!(
        started.elapsed() >= Duration::from_millis(1000),
        "the delay was not applied"
    );
    assert_eq!(
        c.finish(&session, second, "infrastructure_failure", vec![])
            .0,
        200
    );

    let third = c.claim_eventually(&session);
    assert_eq!(third, (job_id, 3));
    assert_eq!(
        c.finish(&session, third, "infrastructure_failure", vec![])
            .0,
        200
    );
    let done = c.status(job_id);
    assert_eq!(str_of(&done, "state"), "finished");
    assert_eq!(str_of(&done, "outcome"), "infrastructure_failure");

    // A build failure, by contrast, is final the first time.
    let other = c.submit("release", "linux", "amd64");
    let job = c.claim(&session).unwrap();
    assert_eq!(c.finish(&session, job, "build_failure", vec![]).0, 200);
    let done = c.status(other);
    assert_eq!(
        (str_of(&done, "state"), str_of(&done, "outcome")),
        ("finished", "build_failure")
    );
}

#[test]
fn cancelling_jobs() {
    let Some(c) = Coordinator::start(&[]) else {
        return;
    };
    let session = c.register("release", "linux", "amd64");

    // A waiting job is cancelled at once and is never handed out.
    let waiting = c.submit("release", "linux", "amd64");
    let (status, reply) = c.admin("POST", &format!("/admin/v1/jobs/{waiting}/cancel"), None);
    assert_eq!(status, 200);
    assert_eq!(
        (str_of(&reply, "state"), str_of(&reply, "outcome")),
        ("finished", "cancelled")
    );
    assert_eq!(c.claim(&session), None);

    // A running job is told at its next report.
    let running = c.submit("release", "linux", "amd64");
    let job = c.claim(&session).unwrap();
    assert_eq!(c.report(&session, job, 0, b"working\n").0, "continue");
    assert_eq!(
        c.admin("POST", &format!("/admin/v1/jobs/{running}/cancel"), None)
            .0,
        200
    );
    assert_eq!(str_of(&c.status(running), "state"), "running");
    assert_eq!(c.report(&session, job, 8, b"").0, "cancel");
    assert_eq!(c.finish(&session, job, "cancelled", vec![]).0, 200);
    let done = c.status(running);
    assert_eq!(
        (str_of(&done, "state"), str_of(&done, "outcome")),
        ("finished", "cancelled")
    );

    assert_eq!(c.admin("POST", "/admin/v1/jobs/999999/cancel", None).0, 404);
}

#[test]
fn artifacts_must_match_what_was_uploaded() {
    let Some(c) = Coordinator::start(&[]) else {
        return;
    };
    let session = c.register("release", "linux", "amd64");
    let job_id = c.submit("release", "linux", "amd64");
    let job = c.claim(&session).unwrap();
    let base = format!("/worker/v1/jobs/{job_id}/attempts/1/artifacts");
    assert_eq!(
        c.call(
            &c.worker_url,
            "PUT",
            &format!("{base}/a.txt"),
            Some(&session),
            Some(b"real")
        )
        .0,
        200
    );

    let entry = |path: &str, size: i64, data: &[u8]| {
        Value::object([
            ("path", Value::from(path)),
            ("size", Value::Int(size)),
            ("sha256", Value::from(hex_encode(&sha256(data)))),
        ])
    };
    assert_eq!(
        c.finish(
            &session,
            job,
            "success",
            vec![entry("never-sent.txt", 4, b"real")]
        )
        .0,
        400
    );
    assert_eq!(
        c.finish(&session, job, "success", vec![entry("a.txt", 4, b"fake")])
            .0,
        400
    );
    assert_eq!(
        c.finish(&session, job, "success", vec![entry("a.txt", 5, b"real")])
            .0,
        400
    );
    assert_eq!(
        str_of(&c.status(job_id), "state"),
        "running",
        "a refused Finish changes nothing"
    );

    // Paths that would escape the job's directory, or mean something
    // special on some platform, are refused.
    for bad in [
        "..%2Fescape",
        "a%5Cb",
        "C:x",
        "a/../../b",
        "trailing.",
        ".hidden/..",
        "%2e%2e/x",
    ] {
        let (status, _) = c.call(
            &c.worker_url,
            "PUT",
            &format!("{base}/{bad}"),
            Some(&session),
            Some(b"x"),
        );
        assert!(status == 400 || status == 404, "{bad} got {status}");
    }
    // Uploading the same path again replaces it.
    assert_eq!(
        c.call(
            &c.worker_url,
            "PUT",
            &format!("{base}/a.txt"),
            Some(&session),
            Some(b"newer")
        )
        .0,
        200
    );
    assert_eq!(
        c.finish(&session, job, "success", vec![entry("a.txt", 5, b"newer")])
            .0,
        200
    );
    let (_, fetched) = c.call(
        &c.admin_url,
        "GET",
        &format!("/admin/v1/jobs/{job_id}/artifacts/a.txt"),
        None,
        None,
    );
    assert_eq!(fetched, b"newer");
    assert_eq!(
        c.call(
            &c.admin_url,
            "GET",
            &format!("/admin/v1/jobs/{job_id}/artifacts/other.txt"),
            None,
            None
        )
        .0,
        404
    );
}

#[test]
fn bad_submissions_are_refused() {
    let Some(c) = Coordinator::start(&[]) else {
        return;
    };
    let with_description = |description: Value| {
        Value::object([
            ("pool", Value::from("release")),
            ("os", Value::from("linux")),
            ("arch", Value::from("amd64")),
            ("description", description),
        ])
    };
    let local_input = Value::object([
        ("image", Value::from("img")),
        ("command", Value::strings(["run"])),
        ("timeout_seconds", Value::Int(60)),
        (
            "inputs",
            Value::Array(vec![Value::object([
                ("name", Value::from("passwd")),
                ("url", Value::from("file:///etc/passwd")),
            ])]),
        ),
    ]);
    for (what, body) in [
        ("a file:// input", with_description(local_input)),
        (
            "no command",
            with_description(Value::object([
                ("image", Value::from("img")),
                ("timeout_seconds", Value::Int(60)),
            ])),
        ),
        (
            "a description that is not an object",
            with_description(Value::from("run")),
        ),
        (
            "an upper-case pool",
            job_request("Release", "linux", "amd64"),
        ),
        ("an empty platform", job_request("release", "", "amd64")),
        (
            "no description",
            Value::object([
                ("pool", Value::from("release")),
                ("os", Value::from("linux")),
                ("arch", Value::from("amd64")),
            ]),
        ),
    ] {
        let (status, reply) = c.admin("POST", "/admin/v1/jobs", Some(body));
        assert_eq!(status, 400, "{what}: {reply:?}");
    }
    let (status, _) = c.call(
        &c.admin_url,
        "POST",
        "/admin/v1/jobs",
        None,
        Some(b"not json"),
    );
    assert_eq!(status, 400);
    assert_eq!(
        c.admin("GET", "/admin/v1/jobs/1", None).0,
        404,
        "nothing was stored"
    );
}

#[test]
fn only_one_coordinator_runs_against_a_database() {
    let Some(c) = Coordinator::start(&[]) else {
        return;
    };
    let mut second = Command::new(env!("CARGO_BIN_EXE_ci-coordinator"))
        .env("CI_DATABASE_URL", &c.database_url)
        .arg("--data-dir")
        .arg(&c.data_dir)
        .arg("--worker-listen")
        .arg("127.0.0.1:0")
        .arg("--admin-listen")
        .arg("127.0.0.1:0")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = second.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = second.kill();
            panic!("a second coordinator kept running beside the first");
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    let mut message = String::new();
    std::io::Read::read_to_string(&mut second.stderr.take().unwrap(), &mut message).unwrap();
    assert!(message.contains("another coordinator"), "{message}");
}

#[test]
fn a_restart_loses_nothing() {
    let Some(mut c) =
        Coordinator::start(&["--lease-seconds", "3", "--report-interval-seconds", "1"])
    else {
        return;
    };
    let session = c.register("release", "linux", "amd64");
    let running = c.submit("release", "linux", "amd64");
    let queued = c.submit("release", "linux", "amd64");
    let job = c.claim(&session).unwrap();
    assert_eq!(
        c.report(&session, job, 0, b"before the restart\n"),
        ("continue".to_string(), 19)
    );

    // The coordinator is down for longer than the lease.
    c.stop();
    thread::sleep(Duration::from_secs(4));
    c.process = Some(c.spawn());
    c.wait_until_listening();

    // The same session still works, the job is still this worker's, and
    // the log carries on where it stopped.
    assert_eq!(str_of(&c.status(running), "state"), "running");
    assert_eq!(
        c.report(&session, job, 19, b"after the restart\n"),
        ("continue".to_string(), 37)
    );
    assert_eq!(c.finish(&session, job, "success", vec![]).0, 200);
    let (_, log) = c.call(
        &c.admin_url,
        "GET",
        &format!("/admin/v1/jobs/{running}/log"),
        None,
        None,
    );
    assert_eq!(log, b"before the restart\nafter the restart\n");

    // The queue survived too.
    assert_eq!(c.claim(&session), Some((queued, 1)));
    c.restart();
    assert_eq!(str_of(&c.status(queued), "state"), "running");
}

#[test]
fn the_admin_api_needs_an_admin_token() {
    let Some(mut c) = Coordinator::start(&[]) else {
        return;
    };
    let session = c.register("release", "linux", "amd64");
    let job_id = c.submit("release", "linux", "amd64");
    let job = c.claim(&session).unwrap();
    assert_eq!(
        c.report(&session, job, 0, b"secret build output\n").0,
        "continue"
    );

    // Every admin path, with no token, a made-up token, a worker's session
    // token, and a token in the wrong scheme.
    let first = c.admin_token();
    let basic = format!("Basic {first}");
    let paths = [
        ("POST", "/admin/v1/jobs".to_string()),
        ("GET", format!("/admin/v1/jobs/{job_id}")),
        ("GET", format!("/admin/v1/jobs/{job_id}/log")),
        ("GET", format!("/admin/v1/jobs/{job_id}/artifacts/x")),
        ("POST", format!("/admin/v1/jobs/{job_id}/cancel")),
        ("POST", "/admin/v1/worker-tokens".to_string()),
        ("POST", "/admin/v1/worker-tokens/1/revoke".to_string()),
        ("POST", "/admin/v1/admin-tokens".to_string()),
        ("POST", "/admin/v1/admin-tokens/1/revoke".to_string()),
        ("GET", "/admin/v1/no-such-path".to_string()),
        ("GET", "/".to_string()),
    ];
    for (method, path) in &paths {
        for token in [None, Some("not-a-token"), Some(session.as_str()), Some("")] {
            let (status, body) = c.raw(&c.admin_url, method, path, token, Some(b"{}"));
            assert_eq!(status, 401, "{method} {path} with token {token:?}");
            assert!(
                !String::from_utf8_lossy(&body).contains("secret"),
                "{method} {path}"
            );
        }
        // "Basic" in place of "Bearer" is not accepted either.
        let url = Url::parse(&format!("{}{path}", c.admin_url)).unwrap();
        let host = url.host_header();
        let response = Connector::plain_only()
            .send(
                &url,
                Request::new(method, &url.target, &host).header("Authorization", &basic),
            )
            .unwrap();
        assert_eq!(response.status, 401, "{method} {path} with Basic");
    }
    // Nothing above had any effect.
    let status = c.status(job_id);
    assert_eq!(str_of(&status, "state"), "running");
    assert_eq!(
        status.get("cancel_requested").unwrap().as_bool(),
        Some(false)
    );

    // With the admin token, an unknown path is simply not found.
    assert_eq!(
        c.call(&c.admin_url, "GET", "/admin/v1/no-such-path", None, None)
            .0,
        404
    );

    // The token is stored only as a hash.
    let mut database =
        ci_postgres::Connection::connect(&ci_postgres::Config::parse(&c.database_url).unwrap())
            .unwrap();
    let stored = database
        .query("SELECT token_hash FROM admin_tokens", &[])
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored.first().unwrap().text(0).unwrap(),
        hex_encode(&sha256(first.as_bytes()))
    );
    drop(database);

    // The token file can be read only by its owner.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(c.data_dir.join("admin-token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // A restart keeps the token: no second one is created.
    c.restart();
    assert_eq!(c.admin_token(), first);
    assert_eq!(str_of(&c.status(job_id), "state"), "running");
}

#[test]
fn admin_tokens_can_be_issued_and_revoked() {
    let Some(mut c) = Coordinator::start(&[]) else {
        return;
    };
    let first = c.admin_token();
    let with = |token: &str, method: &str, path: &str, body: Option<Value>| {
        let text = body.map(|b| b.to_json());
        let (status, bytes) = c.raw(
            &c.admin_url,
            method,
            path,
            Some(token),
            text.as_deref().map(str::as_bytes),
        );
        (status, ci_json::parse_bytes(&bytes).unwrap())
    };
    let name = |text: &str| Some(Value::object([("name", Value::from(text))]));

    let (status, reply) = with(
        &first,
        "POST",
        "/admin/v1/admin-tokens",
        name("second administrator"),
    );
    assert_eq!(status, 201, "{reply:?}");
    let second = str_of(&reply, "token").to_string();
    let second_id = reply.get("token_id").unwrap().as_i64().unwrap();
    assert_ne!(second, first);
    assert_eq!(
        with(&first, "POST", "/admin/v1/admin-tokens", name("")).0,
        400
    );
    assert_eq!(with(&first, "POST", "/admin/v1/admin-tokens", None).0, 400);

    // The new token works, and can revoke the first.
    let job_id = {
        let (status, reply) = with(
            &second,
            "POST",
            "/admin/v1/jobs",
            Some(job_request("release", "linux", "amd64")),
        );
        assert_eq!(status, 201, "{reply:?}");
        reply.get("job_id").unwrap().as_i64().unwrap()
    };
    assert_eq!(
        with(&second, "POST", "/admin/v1/admin-tokens/1/revoke", None).0,
        200
    );
    assert_eq!(
        with(&first, "GET", &format!("/admin/v1/jobs/{job_id}"), None).0,
        401
    );
    assert_eq!(
        with(&second, "GET", &format!("/admin/v1/jobs/{job_id}"), None).0,
        200
    );
    // Revoking twice, or revoking a token that never existed, is reported.
    assert_eq!(
        with(&second, "POST", "/admin/v1/admin-tokens/1/revoke", None).0,
        404
    );
    assert_eq!(
        with(&second, "POST", "/admin/v1/admin-tokens/999/revoke", None).0,
        404
    );
    assert_eq!(
        with(&second, "POST", "/admin/v1/admin-tokens/x/revoke", None).0,
        404
    );

    // A revoked token stays revoked across a restart, and no new first
    // token appears while another token still works.
    let (worker_port, admin_port) = (c.worker_port, c.admin_port);
    c.restart();
    assert_eq!((c.worker_port, c.admin_port), (worker_port, admin_port));
    assert_eq!(
        c.raw(
            &c.admin_url,
            "GET",
            &format!("/admin/v1/jobs/{job_id}"),
            Some(&first),
            None
        )
        .0,
        401
    );
    assert_eq!(
        c.raw(
            &c.admin_url,
            "GET",
            &format!("/admin/v1/jobs/{job_id}"),
            Some(&second),
            None
        )
        .0,
        200
    );
    assert_eq!(c.admin_token(), first, "the token file was not rewritten");

    // If every token is revoked, the next start makes a new first token,
    // so an administrator who can read the data directory can get back in.
    let revoke = format!("/admin/v1/admin-tokens/{second_id}/revoke");
    assert_eq!(
        c.raw(&c.admin_url, "POST", &revoke, Some(&second), None).0,
        200
    );
    assert_eq!(
        c.raw(
            &c.admin_url,
            "GET",
            &format!("/admin/v1/jobs/{job_id}"),
            Some(&second),
            None
        )
        .0,
        401
    );
    c.restart();
    let third = c.admin_token();
    assert_ne!(third, first);
    assert_ne!(third, second);
    assert_eq!(
        c.raw(
            &c.admin_url,
            "GET",
            &format!("/admin/v1/jobs/{job_id}"),
            Some(&third),
            None
        )
        .0,
        200
    );
}

#[test]
fn a_revoked_worker_token_shuts_its_workers_out() {
    let Some(c) = Coordinator::start(&[]) else {
        return;
    };
    let session = c.register("release", "linux", "amd64");
    let other = c.register("release", "linux", "amd64");
    let job_id = c.submit("release", "linux", "amd64");
    let job = c.claim(&session).unwrap();

    // The first registration used worker token 1.
    assert_eq!(
        c.admin("POST", "/admin/v1/worker-tokens/1/revoke", None).0,
        200
    );
    let empty = || Some(Value::object::<&str>([]));
    assert_eq!(
        c.json(
            &c.worker_url,
            "POST",
            "/worker/v1/claim",
            Some(&session),
            empty()
        )
        .0,
        401
    );
    let upload = format!("/worker/v1/jobs/{job_id}/attempts/{}/artifacts/x", job.1);
    assert_eq!(
        c.call(&c.worker_url, "PUT", &upload, Some(&session), Some(b"x"))
            .0,
        401
    );
    // A worker enrolled with a different token is unaffected.
    assert_eq!(c.claim(&other), None);
    assert_eq!(
        c.admin("POST", "/admin/v1/worker-tokens/1/revoke", None).0,
        404
    );
}
