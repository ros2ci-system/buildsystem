// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The agent as a worker: it registers with a coordinator, then claims
//! jobs, runs them, and reports on them, for as long as it lives.
//!
//! The worker makes every connection. The coordinator never reaches in.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ci_container::Client;
use ci_crypto::{base64_encode, hex_encode, Sha256};
use ci_http::{Body, Request};
use ci_json::Value;
use ci_net::{Connector, Url};
use ci_protocol::{
    artifact_path, is_safe_artifact_path, Action, JobDescription, Outcome, PATH_CLAIM, PATH_FINISH,
    PATH_REGISTER, PATH_REPORT, PROTOCOL_VERSION,
};

use crate::run::{self, Control};

/// The most log sent in one Report. The rest follows in the next.
const MAX_LOG_CHUNK: usize = 1024 * 1024;

/// The longest a reply from the coordinator may be.
const MAX_REPLY: usize = 64 * 1024 * 1024;

/// How long to keep trying to deliver a finished job's results while the
/// coordinator cannot be reached.
const DELIVERY_PATIENCE: Duration = Duration::from_secs(3600);

pub struct Worker {
    pub coordinator: Url,
    pub connector: Connector,
    pub enrolment_token: String,
    pub runtime: Client,
    pub work_dir: PathBuf,
}

struct Session {
    token: String,
    claim_interval: Duration,
}

struct Claimed {
    job_id: i64,
    attempt: i64,
    description: Value,
    report_interval: Duration,
}

/// A call that got an answer: the status and the JSON body.
type Reply = (u16, Value);

impl Worker {
    /// Runs until the process is killed. Returns only on an error that
    /// trying again cannot fix, such as a refused enrolment token.
    pub fn run(&self) -> Result<(), String> {
        fs::create_dir_all(&self.work_dir)
            .map_err(|e| format!("cannot create {}: {e}", self.work_dir.display()))?;
        let identity = self.identity()?;
        self.clean_up_after_a_crash(&identity);
        let mut session = self.register()?;
        let mut failures = 0u32;
        loop {
            match self.post(PATH_CLAIM, &session.token, &Value::object::<&str>([])) {
                Ok((200, reply)) => {
                    failures = 0;
                    match parse_claim(&reply) {
                        Ok(Some(job)) => self.run_claimed(&session, &identity, job),
                        Ok(None) => thread::sleep(session.claim_interval),
                        Err(message) => {
                            eprintln!("warning: the coordinator's reply to Claim made no sense: {message}");
                            thread::sleep(session.claim_interval);
                        }
                    }
                }
                Ok((401, _)) => {
                    // The coordinator no longer knows this session, for
                    // example because its database was replaced.
                    eprintln!("the coordinator no longer knows this worker; registering again");
                    session = self.register()?;
                }
                Ok((status, reply)) => {
                    eprintln!(
                        "warning: Claim was answered with {status}: {}",
                        error_text(&reply)
                    );
                    thread::sleep(backoff(&mut failures));
                }
                Err(message) => {
                    eprintln!("warning: cannot reach the coordinator: {message}");
                    thread::sleep(backoff(&mut failures));
                }
            }
        }
    }

    /// A random name for this agent, kept in its work directory so that
    /// it is the same after a restart. Containers are labelled with it.
    fn identity(&self) -> Result<String, String> {
        let path = self.work_dir.join("agent-id");
        if let Ok(text) = fs::read_to_string(&path) {
            let text = text.trim();
            if text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Ok(text.to_string());
            }
        }
        let identity = ci_crypto::random_token(16);
        fs::write(&path, &identity).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(identity)
    }

    /// Removes what a previous run of this agent left behind if it was
    /// killed: its containers, and the directories of its jobs. Those
    /// jobs have been, or will be, handed to a worker again.
    fn clean_up_after_a_crash(&self, identity: &str) {
        match self
            .runtime
            .containers_with_label(run::OWNER_LABEL, identity)
        {
            Ok(ids) => {
                for id in ids {
                    match self.runtime.remove_container(&id) {
                        Ok(()) => eprintln!("removed a container left by an earlier run: {id}"),
                        Err(e) => eprintln!("warning: could not remove container {id}: {e}"),
                    }
                }
            }
            Err(e) => {
                eprintln!("warning: could not look for containers left by an earlier run: {e}")
            }
        }
        if let Ok(entries) = fs::read_dir(&self.work_dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with("job-") {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
    }

    fn register(&self) -> Result<Session, String> {
        let mut failures = 0u32;
        loop {
            // What the worker can run is decided by its container
            // runtime, not by the machine the agent happens to be on.
            let version = match self.runtime.version() {
                Ok(version) => version,
                Err(e) => {
                    eprintln!("warning: {e}");
                    thread::sleep(backoff(&mut failures));
                    continue;
                }
            };
            let field = |name: &str| {
                version
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::to_ascii_lowercase)
                    .ok_or_else(|| format!("the container runtime did not report its {name}"))
            };
            let body = Value::object([
                ("protocol_version", Value::Int(PROTOCOL_VERSION)),
                ("os", Value::from(field("Os")?)),
                ("arch", Value::from(field("Arch")?)),
            ]);
            match self.post(PATH_REGISTER, &self.enrolment_token, &body) {
                Ok((200, reply)) => {
                    let token = reply
                        .get("session_token")
                        .and_then(Value::as_str)
                        .ok_or("the coordinator's reply to Register has no session token")?
                        .to_string();
                    let claim_interval = seconds(&reply, "claim_interval_seconds", 3);
                    eprintln!(
                        "registered as worker {} in pool {}",
                        reply.get("worker_id").and_then(Value::as_i64).unwrap_or(0),
                        reply.get("pool").and_then(Value::as_str).unwrap_or("?"),
                    );
                    return Ok(Session {
                        token,
                        claim_interval,
                    });
                }
                Ok((status @ (400 | 401 | 403), reply)) => {
                    return Err(format!(
                        "the coordinator refused to register this worker ({status}): {}",
                        error_text(&reply)
                    ));
                }
                Ok((status, reply)) => {
                    eprintln!(
                        "warning: Register was answered with {status}: {}",
                        error_text(&reply)
                    );
                }
                Err(message) => eprintln!("warning: cannot reach the coordinator: {message}"),
            }
            thread::sleep(backoff(&mut failures));
        }
    }

    fn run_claimed(&self, session: &Session, identity: &str, job: Claimed) {
        eprintln!("job {} attempt {}: starting", job.job_id, job.attempt);
        let job_dir = self
            .work_dir
            .join(format!("job-{}-{}", job.job_id, job.attempt));
        let _ = fs::remove_dir_all(&job_dir);

        let mut reporter = Reporter {
            worker: self,
            token: &session.token,
            job_id: job.job_id,
            attempt: job.attempt,
            log_path: job_dir.join("log.txt"),
            offset: 0,
            interval: job.report_interval,
            last: None,
            action: Action::Continue,
        };

        let result = match JobDescription::parse(&job.description.to_json()) {
            Ok(description) => run::run_job(
                &self.runtime,
                &run::Fetcher {
                    connector: &self.connector,
                    stored: Some(run::StoredAccess {
                        coordinator: &self.coordinator,
                        token: &session.token,
                        job_id: job.job_id,
                        attempt: job.attempt,
                    }),
                },
                &description,
                &job_dir,
                Some(identity),
                &mut || reporter.tick(),
            )
            .map_err(|e| format!("cannot use {}: {e}", job_dir.display())),
            Err(message) => Err(format!("the job description is not valid: {message}")),
        };
        let mut result = match result {
            Ok(result) => result,
            Err(message) => run::JobResult {
                outcome: Outcome::InfrastructureFailure,
                exit_code: None,
                message,
                image_id: String::new(),
                duration: Duration::ZERO,
                outputs: Vec::new(),
            },
        };

        if reporter.action == Action::Abandon {
            // The job belongs to someone else now. Say nothing more.
            eprintln!(
                "job {} attempt {}: abandoned on the coordinator's instruction",
                job.job_id, job.attempt
            );
            let _ = fs::remove_dir_all(&job_dir);
            return;
        }

        // Deliver what is left of the log, then the outputs, then the
        // result. Each step keeps the lease alive.
        let deadline = Instant::now() + DELIVERY_PATIENCE;
        let mut delivered = reporter.flush(deadline);
        let mut artifacts = Vec::new();
        if delivered {
            for output in &result.outputs {
                // Outputs live under "out/" in the job directory. That
                // prefix is not part of the artifact's name.
                let relative = output.to_string_lossy().replace('\\', "/");
                let name = relative
                    .strip_prefix("out/")
                    .unwrap_or(&relative)
                    .to_string();
                if !is_safe_artifact_path(&name) {
                    eprintln!("warning: skipping output with an unusable name: {name:?}");
                    continue;
                }
                match self.upload(&session.token, &job, &name, &job_dir.join(output), deadline) {
                    Upload::Stored(entry) => artifacts.push(entry),
                    Upload::Abandon => {
                        delivered = false;
                        reporter.action = Action::Abandon;
                        break;
                    }
                    Upload::Failed(message) => {
                        // The job itself may have passed, but a result
                        // without its outputs is not a result.
                        result.outcome = Outcome::InfrastructureFailure;
                        result.exit_code = None;
                        result.message = format!("uploading {name}: {message}");
                        artifacts.clear();
                        break;
                    }
                }
                reporter.heartbeat();
                if reporter.action == Action::Abandon {
                    delivered = false;
                    break;
                }
            }
        }
        if delivered {
            self.finish(&session.token, &job, &result, artifacts, deadline);
        } else {
            eprintln!(
                "job {} attempt {}: could not deliver its results",
                job.job_id, job.attempt
            );
        }
        let _ = fs::remove_dir_all(&job_dir);
    }

    fn upload(
        &self,
        token: &str,
        job: &Claimed,
        name: &str,
        file: &Path,
        deadline: Instant,
    ) -> Upload {
        let (size, digest) = match hash_file(file) {
            Ok(result) => result,
            Err(e) => return Upload::Failed(e.to_string()),
        };
        let url = self.coordinator.with_target(&artifact_path(
            job.job_id,
            job.attempt,
            &encode_path(name),
        ));
        let host = url.host_header();
        let authorization = format!("Bearer {token}");
        let mut failures = 0u32;
        loop {
            let attempt = File::open(file)
                .map_err(|e| e.to_string())
                .and_then(|mut source| {
                    let request = Request::new("PUT", &url.target, &host)
                        .header("Authorization", &authorization)
                        .header("Content-Type", "application/octet-stream")
                        .body(Body::Sized(&mut source, size));
                    let mut response = self
                        .connector
                        .send(&url, request)
                        .map_err(|e| e.to_string())?;
                    let body = response.body_bytes(MAX_REPLY).map_err(|e| e.to_string())?;
                    Ok((
                        response.status,
                        ci_json::parse_bytes(&body).unwrap_or(Value::Null),
                    ))
                });
            match attempt {
                Ok((200, reply)) => {
                    // The coordinator hashes what it received. If that
                    // differs, the file was damaged on the way.
                    let stored = reply.get("sha256").and_then(Value::as_str).unwrap_or("");
                    if !stored.eq_ignore_ascii_case(&digest) {
                        return Upload::Failed(
                            "the coordinator received different bytes".to_string(),
                        );
                    }
                    return Upload::Stored(Value::object([
                        ("path", Value::from(name)),
                        ("size", Value::Int(size as i64)),
                        ("sha256", Value::from(digest)),
                    ]));
                }
                Ok((409, _)) => return Upload::Abandon,
                Ok((status @ (400 | 413), reply)) => {
                    return Upload::Failed(format!("refused with {status}: {}", error_text(&reply)))
                }
                Ok((status, _)) => {
                    eprintln!("warning: uploading {name} was answered with {status}")
                }
                Err(message) => eprintln!("warning: uploading {name}: {message}"),
            }
            if Instant::now() >= deadline {
                return Upload::Failed("the coordinator could not be reached".to_string());
            }
            thread::sleep(backoff(&mut failures));
        }
    }

    fn finish(
        &self,
        token: &str,
        job: &Claimed,
        result: &run::JobResult,
        artifacts: Vec<Value>,
        deadline: Instant,
    ) {
        let body = Value::object([
            ("job_id", Value::Int(job.job_id)),
            ("attempt", Value::Int(job.attempt)),
            ("outcome", Value::from(result.outcome.as_str())),
            (
                "exit_code",
                result.exit_code.map(Value::Int).unwrap_or(Value::Null),
            ),
            ("message", Value::from(result.message.as_str())),
            ("image_id", Value::from(result.image_id.as_str())),
            ("artifacts", Value::Array(artifacts)),
        ]);
        let mut failures = 0u32;
        loop {
            match self.post(PATH_FINISH, token, &body) {
                Ok((200, _)) => {
                    eprintln!(
                        "job {} attempt {}: {}{}",
                        job.job_id,
                        job.attempt,
                        result.outcome.as_str(),
                        if result.message.is_empty() {
                            String::new()
                        } else {
                            format!(" ({})", result.message)
                        }
                    );
                    return;
                }
                Ok((status @ (400 | 409), reply)) => {
                    eprintln!(
                        "job {} attempt {}: the coordinator did not accept the result ({status}): {}",
                        job.job_id,
                        job.attempt,
                        error_text(&reply)
                    );
                    return;
                }
                Ok((status, _)) => eprintln!("warning: Finish was answered with {status}"),
                Err(message) => eprintln!("warning: cannot reach the coordinator: {message}"),
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "job {} attempt {}: gave up delivering the result",
                    job.job_id, job.attempt
                );
                return;
            }
            thread::sleep(backoff(&mut failures));
        }
    }

    /// Sends a JSON request and returns the status and JSON reply.
    fn post(&self, path: &str, token: &str, body: &Value) -> Result<Reply, String> {
        let url = self.coordinator.with_target(path);
        let host = url.host_header();
        let authorization = format!("Bearer {token}");
        let text = body.to_json();
        let request = Request::new("POST", &url.target, &host)
            .header("Authorization", &authorization)
            .header("Content-Type", "application/json")
            .body(Body::Bytes(text.as_bytes()));
        let mut response = self
            .connector
            .send(&url, request)
            .map_err(|e| e.to_string())?;
        let bytes = response.body_bytes(MAX_REPLY).map_err(|e| e.to_string())?;
        let value =
            ci_json::parse_bytes(&bytes).map_err(|e| format!("the reply was not JSON: {e}"))?;
        Ok((response.status, value))
    }
}

enum Upload {
    Stored(Value),
    Abandon,
    Failed(String),
}

/// Sends the job's log to the coordinator as it grows, and keeps the
/// coordinator's latest instruction.
struct Reporter<'a> {
    worker: &'a Worker,
    token: &'a str,
    job_id: i64,
    attempt: i64,
    log_path: PathBuf,
    /// How much of the log the coordinator has stored.
    offset: u64,
    interval: Duration,
    last: Option<Instant>,
    action: Action,
}

impl Reporter<'_> {
    /// Called often while the job runs. Sends a Report when one is due.
    fn tick(&mut self) -> Control {
        if self.action == Action::Continue
            && self.last.is_none_or(|last| last.elapsed() >= self.interval)
        {
            self.report();
        }
        match self.action {
            Action::Continue => Control::Continue,
            Action::Cancel | Action::Abandon => Control::Stop,
        }
    }

    /// Sends a Report now, whatever the interval, to keep the lease.
    fn heartbeat(&mut self) {
        if self.action != Action::Abandon {
            self.report();
        }
    }

    /// Sends Reports until the coordinator has the whole log. Returns
    /// `false` if that could not be done.
    fn flush(&mut self, deadline: Instant) -> bool {
        let mut failures = 0u32;
        loop {
            let sent = self.report();
            if self.action == Action::Abandon {
                return false;
            }
            let length = fs::metadata(&self.log_path).map(|m| m.len()).unwrap_or(0);
            if sent && self.offset >= length {
                return true;
            }
            if !sent {
                if Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(backoff(&mut failures));
            }
        }
    }

    /// Sends one Report with the next piece of log. Returns whether the
    /// coordinator answered.
    fn report(&mut self) -> bool {
        self.last = Some(Instant::now());
        let chunk = read_from(&self.log_path, self.offset, MAX_LOG_CHUNK).unwrap_or_default();
        let body = Value::object([
            ("job_id", Value::Int(self.job_id)),
            ("attempt", Value::Int(self.attempt)),
            ("log_offset", Value::Int(self.offset as i64)),
            ("log_base64", Value::from(base64_encode(&chunk))),
        ]);
        match self.worker.post(PATH_REPORT, self.token, &body) {
            Ok((200, reply)) => {
                // The coordinator says how much it holds. That is where
                // the next chunk starts, even if a reply went missing.
                if let Some(stored) = reply.get("log_offset").and_then(Value::as_u64) {
                    self.offset = stored;
                }
                match reply
                    .get("action")
                    .and_then(Value::as_str)
                    .and_then(Action::parse)
                {
                    Some(Action::Continue) => {}
                    // A cancel is kept: later replies do not undo it.
                    Some(Action::Cancel) if self.action == Action::Continue => {
                        self.action = Action::Cancel
                    }
                    Some(Action::Cancel) => {}
                    Some(Action::Abandon) => self.action = Action::Abandon,
                    None => {
                        eprintln!("warning: the coordinator's reply to Report had no usable action")
                    }
                }
                true
            }
            Ok((status, reply)) => {
                eprintln!(
                    "warning: Report was answered with {status}: {}",
                    error_text(&reply)
                );
                false
            }
            Err(message) => {
                // The job keeps running. If the coordinator stays out of
                // reach, the log is sent when it comes back.
                eprintln!("warning: cannot reach the coordinator: {message}");
                false
            }
        }
    }
}

fn parse_claim(reply: &Value) -> Result<Option<Claimed>, String> {
    let job = reply.get("job").ok_or("no \"job\" member")?;
    if job.is_null() {
        return Ok(None);
    }
    Ok(Some(Claimed {
        job_id: job
            .get("job_id")
            .and_then(Value::as_i64)
            .ok_or("no job id")?,
        attempt: job
            .get("attempt")
            .and_then(Value::as_i64)
            .ok_or("no attempt number")?,
        description: job.get("description").cloned().ok_or("no description")?,
        report_interval: seconds(job, "report_interval_seconds", 10),
    }))
}

fn seconds(value: &Value, name: &str, default: u64) -> Duration {
    Duration::from_secs(
        value
            .get(name)
            .and_then(Value::as_u64)
            .filter(|n| (1..=3600).contains(n))
            .unwrap_or(default),
    )
}

fn error_text(reply: &Value) -> String {
    reply
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("no reason given")
        .to_string()
}

/// One second, then two, then four, up to a minute.
fn backoff(failures: &mut u32) -> Duration {
    let delay = Duration::from_secs(1u64 << (*failures).min(6)).min(Duration::from_secs(60));
    *failures = failures.saturating_add(1);
    delay
}

/// Reads up to `limit` bytes of a file starting at `offset`.
fn read_from(path: &Path, offset: u64, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut out = Vec::new();
    file.take(limit as u64).read_to_end(&mut out)?;
    Ok(out)
}

fn hash_file(path: &Path) -> std::io::Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        size += n as u64;
    }
    Ok((size, hex_encode(&hash.finish())))
}

/// Percent-encodes each segment of a relative path for use in a URL.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(ci_http::percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_stops_at_a_minute() {
        let mut failures = 0;
        let delays: Vec<u64> = (0..9).map(|_| backoff(&mut failures).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn claim_replies() {
        assert!(parse_claim(&ci_json::parse(r#"{"job": null}"#).unwrap())
            .unwrap()
            .is_none());
        let job = parse_claim(
            &ci_json::parse(r#"{"job": {"job_id": 7, "attempt": 2, "description": {"x": 1}, "report_interval_seconds": 5}}"#)
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (job.job_id, job.attempt, job.report_interval),
            (7, 2, Duration::from_secs(5))
        );
        for bad in [
            r#"{}"#,
            r#"{"job": {}}"#,
            r#"{"job": {"job_id": 7}}"#,
            r#"{"job": {"job_id": 7, "attempt": 1}}"#,
        ] {
            assert!(parse_claim(&ci_json::parse(bad).unwrap()).is_err(), "{bad}");
        }
    }

    #[test]
    fn intervals_fall_back_when_missing_or_absurd() {
        let reply = ci_json::parse(r#"{"a": 5, "zero": 0, "huge": 999999, "text": "x"}"#).unwrap();
        assert_eq!(seconds(&reply, "a", 10), Duration::from_secs(5));
        for name in ["zero", "huge", "text", "missing"] {
            assert_eq!(seconds(&reply, name, 10), Duration::from_secs(10), "{name}");
        }
    }

    #[test]
    fn reading_a_file_from_an_offset() {
        let path = std::env::temp_dir().join(format!("ci_agent_read_from_{}", std::process::id()));
        fs::write(&path, b"0123456789").unwrap();
        assert_eq!(read_from(&path, 0, 4).unwrap(), b"0123");
        assert_eq!(read_from(&path, 4, 100).unwrap(), b"456789");
        assert_eq!(read_from(&path, 10, 100).unwrap(), b"");
        assert_eq!(read_from(&path, 50, 100).unwrap(), b"");
        assert_eq!(hash_file(&path).unwrap().0, 10);
        fs::remove_file(&path).unwrap();
        assert!(read_from(&path, 0, 4).is_err());
    }

    #[test]
    fn artifact_names_are_encoded_per_segment() {
        assert_eq!(encode_path("sub dir/file+1.txt"), "sub%20dir/file%2B1.txt");
    }
}
