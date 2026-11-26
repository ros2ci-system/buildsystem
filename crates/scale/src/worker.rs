// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Stand-in workers: they speak the worker protocol as agents do, and
//! answer each job at once with what the job would have left behind.
//! Beside them, a stand-in for the rosdistro's git server.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ci_crypto::{base64_encode, hex_encode, sha256};
use ci_http::{Body, Request};
use ci_json::Value;
use ci_net::{Connector, Url};

use crate::answers::{self, Files};
use crate::distribution::Distribution;

/// One group of workers: where they register and how many there are.
#[derive(Debug, Clone)]
pub struct Group {
    pub os: String,
    pub arch: String,
    pub count: usize,
    /// The enrolment token, which also decides the pool.
    pub token: String,
}

#[derive(Default)]
pub struct Counters {
    pub checks: AtomicU64,
    pub archives: AtomicU64,
    pub builds: AtomicU64,
    pub refused: AtomicU64,
    /// Calls that did not get through and were made again.
    pub retried: AtomicU64,
    /// Calls the coordinator answered with an error of its own, which
    /// were made again too.
    pub errors: AtomicU64,
}

pub struct Shared {
    pub coordinator: Url,
    pub connector: Connector,
    pub distribution_path: PathBuf,
    pub distribution: RwLock<Arc<Distribution>>,
    pub job_time: Duration,
    pub counters: Counters,
}

impl Shared {
    fn distribution(&self) -> Arc<Distribution> {
        Arc::clone(&self.distribution.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Reads the distribution again whenever its file changes.
    pub fn follow_distribution(self: &Arc<Self>) {
        let shared = Arc::clone(self);
        thread::spawn(move || {
            let modified = |shared: &Shared| -> Option<SystemTime> {
                fs::metadata(&shared.distribution_path)
                    .ok()?
                    .modified()
                    .ok()
            };
            let mut seen = modified(&shared);
            loop {
                thread::sleep(Duration::from_millis(500));
                let now = modified(&shared);
                if now == seen {
                    continue;
                }
                let read = fs::read_to_string(&shared.distribution_path)
                    .map_err(|e| e.to_string())
                    .and_then(|text| Distribution::parse(&text));
                match read {
                    Ok(distribution) => {
                        eprintln!(
                            "the distribution is now at commit {}",
                            distribution.commit()
                        );
                        *shared
                            .distribution
                            .write()
                            .unwrap_or_else(|e| e.into_inner()) = Arc::new(distribution);
                        seen = now;
                    }
                    // Probably half written; the next look reads it whole.
                    Err(message) => eprintln!("cannot read the distribution: {message}"),
                }
            }
        });
    }

    /// Answers the first request of a git fetch with one branch, `main`,
    /// at the distribution's commit.
    pub fn serve_git_refs(self: &Arc<Self>, listener: TcpListener) {
        let shared = Arc::clone(self);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let commit = shared.distribution().commit();
                let packet = |text: String| format!("{:04x}{text}", text.len() + 4);
                let body = [
                    packet("# service=git-upload-pack\n".to_string()),
                    "0000".to_string(),
                    packet(format!("{commit} HEAD\0multi_ack\n")),
                    packet(format!("{commit} refs/heads/main\n")),
                    "0000".to_string(),
                ]
                .concat();
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
    }

    /// Makes a call to the worker API. A call that does not get through
    /// is made again, as an agent would, and counted.
    fn call(&self, method: &str, path: &str, token: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let url = self.coordinator.with_target(path);
        let host = url.host_header();
        let authorization = format!("Bearer {token}");
        let mut wait = Duration::from_millis(100);
        loop {
            let request = Request::new(method, &url.target, &host)
                .header("Authorization", &authorization)
                .body(Body::Bytes(body));
            let reply = self
                .connector
                .send(&url, request)
                .map_err(|e| e.to_string())
                .and_then(|mut response| {
                    let status = response.status;
                    let bytes = response.body_bytes(256 << 20).map_err(|e| e.to_string())?;
                    Ok((status, bytes))
                });
            match reply {
                Ok((status, bytes)) if status < 500 => return (status, bytes),
                failed => {
                    let counter = match failed {
                        Ok(_) => &self.counters.errors,
                        Err(_) => &self.counters.retried,
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                    thread::sleep(wait);
                    wait = (wait * 2).min(Duration::from_secs(5));
                }
            }
        }
    }

    fn post(&self, path: &str, token: &str, body: Value) -> Result<Value, String> {
        let (status, bytes) = self.call("POST", path, token, body.to_json().as_bytes());
        let reply = ci_json::parse_bytes(&bytes).map_err(|e| format!("{path}: {e}"))?;
        if status != 200 {
            return Err(format!(
                "{path}: the coordinator answered {status}: {}",
                reply.get("error").and_then(Value::as_str).unwrap_or("")
            ));
        }
        Ok(reply)
    }

    /// One worker: registers, then takes and answers jobs for ever.
    pub fn work(&self, group: &Group) -> Result<(), String> {
        let reply = self.post(
            "/worker/v1/register",
            &group.token,
            Value::object([
                ("protocol_version", Value::Int(1)),
                ("os", Value::from(group.os.as_str())),
                ("arch", Value::from(group.arch.as_str())),
            ]),
        )?;
        let session = reply
            .get("session_token")
            .and_then(Value::as_str)
            .ok_or("the coordinator gave no session token")?
            .to_string();
        let idle = Duration::from_secs(
            reply
                .get("claim_interval_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(3),
        );
        loop {
            let reply = self.post("/worker/v1/claim", &session, Value::object::<&str>([]))?;
            let Some(job) = reply.get("job").filter(|job| !job.is_null()) else {
                thread::sleep(idle);
                continue;
            };
            let id = (
                job.get("job_id").and_then(Value::as_i64).unwrap_or(0),
                job.get("attempt").and_then(Value::as_i64).unwrap_or(0),
            );
            let null = Value::Null;
            let parameters = job
                .get("description")
                .and_then(|description| description.get("parameters"))
                .unwrap_or(&null);
            let interval = Duration::from_secs(
                job.get("report_interval_seconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(10),
            );
            self.run(&session, id, parameters, &group.arch, interval)?;
        }
    }

    fn run(
        &self,
        session: &str,
        (job_id, attempt): (i64, i64),
        parameters: &Value,
        arch: &str,
        report_interval: Duration,
    ) -> Result<(), String> {
        let distribution = self.distribution();
        let kind = parameters.get("job").and_then(Value::as_str).unwrap_or("");
        let (answer, counter): (Result<Files, String>, _) = match kind {
            "release_check" => (
                Ok(answers::release_check(&distribution, parameters)),
                &self.counters.checks,
            ),
            "source_archive" => (
                answers::source_archive(&distribution, parameters),
                &self.counters.archives,
            ),
            _ => (
                answers::package_build(&distribution, parameters, arch),
                &self.counters.builds,
            ),
        };
        let report = |offset: i64, chunk: &[u8]| {
            self.post(
                "/worker/v1/report",
                session,
                Value::object([
                    ("job_id", Value::Int(job_id)),
                    ("attempt", Value::Int(attempt)),
                    ("log_offset", Value::Int(offset)),
                    ("log_base64", Value::from(base64_encode(chunk))),
                ]),
            )
        };
        let log = match &answer {
            Ok(files) => format!("stand-in worker: {} files, no work done\n", files.len()),
            Err(message) => format!("stand-in worker: {message}\n"),
        };
        report(0, log.as_bytes())?;
        // A job that takes time reports while it runs, as a real one does.
        let end = Instant::now() + self.job_time;
        while Instant::now() < end {
            thread::sleep(report_interval.min(end - Instant::now()));
            report(log.len() as i64, b"")?;
        }
        let mut listed = Vec::new();
        for (path, data) in answer.as_deref().unwrap_or_default() {
            let target = format!("/worker/v1/jobs/{job_id}/attempts/{attempt}/artifacts/{path}");
            let (status, _) = self.call("PUT", &target, session, data);
            if status != 200 {
                return Err(format!(
                    "uploading {path}: the coordinator answered {status}"
                ));
            }
            listed.push(Value::object([
                ("path", Value::from(path.as_str())),
                ("size", Value::Int(data.len() as i64)),
                ("sha256", Value::from(hex_encode(&sha256(data)))),
            ]));
        }
        let outcome = if answer.is_ok() {
            "success"
        } else {
            "build_failure"
        };
        self.post(
            "/worker/v1/finish",
            session,
            Value::object([
                ("job_id", Value::Int(job_id)),
                ("attempt", Value::Int(attempt)),
                ("outcome", Value::from(outcome)),
                ("exit_code", Value::Int(if answer.is_ok() { 0 } else { 1 })),
                (
                    "message",
                    Value::from(answer.as_ref().err().map_or("", String::as_str)),
                ),
                ("image_id", Value::from("sha256:stand-in")),
                ("artifacts", Value::Array(listed)),
            ]),
        )?;
        if answer.is_ok() {
            counter.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.refused.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}
