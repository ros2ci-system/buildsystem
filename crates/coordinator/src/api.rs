// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The coordinator's two HTTP interfaces.
//!
//! The **worker API** is what workers call: Register, Claim, Report,
//! Finish and artifact upload. Every call but Register needs the session
//! token that Register returned.
//!
//! The **admin API** submits and inspects jobs and issues tokens. Every
//! call needs an admin token. It listens on its own address, which should
//! stay private as a second line of defence.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ci_crypto::{base64_decode, hex_encode, random_token, sha256, Sha256};
use ci_http::server::{path_segments, Request, Response};
use ci_json::Value;
use ci_postgres::Error as DbError;
use ci_protocol::job::{is_plain_file_name, set_context_sha256};
use ci_protocol::{
    is_safe_artifact_path, Action, Image, JobDescription, Outcome, Source, PROTOCOL_VERSION,
};

use crate::db::{self, Artifact, Db, Requeue, Worker};

/// Largest JSON request body accepted. A Report carries a chunk of log,
/// so this also bounds how much log one Report can deliver.
const MAX_JSON_BODY: usize = 8 * 1024 * 1024;

pub struct Settings {
    pub data_dir: PathBuf,
    /// How long a job stays claimed without a Report.
    pub lease_seconds: i64,
    /// How often workers are asked to Report.
    pub report_interval_seconds: i64,
    /// How often idle workers are asked to Claim.
    pub claim_interval_seconds: i64,
    /// A job is given up on after this many infrastructure failures.
    pub max_infrastructure_failures: i64,
    /// The delay before the first retry; it doubles each time.
    pub retry_backoff_seconds: i64,
    /// Largest single artifact accepted.
    pub max_artifact_bytes: u64,
}

pub struct Coordinator {
    pub db: Db,
    pub settings: Settings,
}

/// An error reply: a status and a message for the caller.
struct Reject(u16, String);

impl From<DbError> for Reject {
    fn from(e: DbError) -> Reject {
        // The detail goes to the coordinator's own log, not to the caller.
        eprintln!("error: database: {e}");
        Reject(500, "internal error".to_string())
    }
}

impl From<io::Error> for Reject {
    fn from(e: io::Error) -> Reject {
        eprintln!("error: storage: {e}");
        Reject(500, "internal error".to_string())
    }
}

fn reject(status: u16, message: &str) -> Reject {
    Reject(status, message.to_string())
}

fn to_response(result: Result<Response, Reject>) -> Response {
    match result {
        Ok(response) => response,
        Err(Reject(status, message)) => Response::json(
            status,
            Value::object([("error", Value::from(message))]).to_json(),
        ),
    }
}

fn json_ok(value: Value) -> Response {
    Response::json(200, value.to_json())
}

fn read_json(request: &mut Request<'_>) -> Result<Value, Reject> {
    let body = request
        .body_bytes(MAX_JSON_BODY)
        .map_err(|_| reject(400, "could not read the request body"))?
        .ok_or_else(|| reject(413, "the request body is too large"))?;
    let value = ci_json::parse_bytes(&body).map_err(|e| Reject(400, e.to_string()))?;
    if value.as_object().is_none() {
        return Err(reject(400, "the request body must be a JSON object"));
    }
    Ok(value)
}

fn str_field<'a>(value: &'a Value, name: &str) -> Result<&'a str, Reject> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| Reject(400, format!("\"{name}\" must be a string")))
}

fn int_field(value: &Value, name: &str) -> Result<i64, Reject> {
    value
        .get(name)
        .and_then(Value::as_i64)
        .ok_or_else(|| Reject(400, format!("\"{name}\" must be a whole number")))
}

/// A short name used to match jobs to workers: a pool, an operating
/// system or an architecture.
fn is_label(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Tokens are stored only as hashes, so a copy of the database does not
/// give anyone a working token.
fn token_hash(token: &str) -> String {
    hex_encode(&sha256(token.as_bytes()))
}

impl Coordinator {
    // -----------------------------------------------------------------
    // Worker API
    // -----------------------------------------------------------------

    pub fn handle_worker(&self, request: &mut Request<'_>) -> Response {
        to_response(self.route_worker(request))
    }

    fn route_worker(&self, request: &mut Request<'_>) -> Result<Response, Reject> {
        let path = request.path.clone();
        let segments = path_segments(&path);
        match (request.method.as_str(), segments.as_slice()) {
            ("POST", ["worker", "v1", "register"]) => self.register(request),
            ("POST", ["worker", "v1", "claim"]) => {
                let worker = self.authenticate(request)?;
                self.claim(&worker)
            }
            ("POST", ["worker", "v1", "report"]) => {
                let worker = self.authenticate(request)?;
                self.report(&worker, request)
            }
            ("POST", ["worker", "v1", "finish"]) => {
                let worker = self.authenticate(request)?;
                self.finish(&worker, request)
            }
            (
                "PUT",
                ["worker", "v1", "jobs", job_id, "attempts", attempt, "artifacts", rest @ ..],
            ) if !rest.is_empty() => {
                let worker = self.authenticate(request)?;
                let job_id: i64 = job_id.parse().map_err(|_| reject(404, "no such job"))?;
                let attempt: i64 = attempt
                    .parse()
                    .map_err(|_| reject(404, "no such attempt"))?;
                self.upload_artifact(&worker, job_id, attempt, &rest.join("/"), request)
            }
            ("GET", ["worker", "v1", "jobs", job_id, "attempts", attempt, "files", name]) => {
                let worker = self.authenticate(request)?;
                let job_id: i64 = job_id.parse().map_err(|_| reject(404, "no such job"))?;
                let attempt: i64 = attempt
                    .parse()
                    .map_err(|_| reject(404, "no such attempt"))?;
                self.download_stored_file(&worker, job_id, attempt, name)
            }
            _ => Err(reject(404, "not found")),
        }
    }

    /// Serves a stored file to the worker running a job that reads it.
    /// No other worker, and no other job, can fetch it: that is what
    /// keeps a private input, such as a license, to the jobs that were
    /// given it.
    fn download_stored_file(
        &self,
        worker: &Worker,
        job_id: i64,
        attempt: i64,
        name: &str,
    ) -> Result<Response, Reject> {
        let description = self
            .db
            .held_job_description(job_id, worker.id, attempt)?
            .ok_or_else(|| reject(409, "this job no longer belongs to this worker"))?;
        let job = JobDescription::parse(&description).map_err(|e| {
            eprintln!("error: job {job_id} has an unreadable description: {e}");
            reject(500, "internal error")
        })?;
        if !job.stored_files().contains(&name) {
            return Err(reject(404, "this job does not read that file"));
        }
        let stored = self
            .db
            .stored_file(name)?
            .ok_or_else(|| reject(404, "no such stored file"))?;
        let file = File::open(self.stored_file_path(name))?;
        Ok(Response::file(
            "application/octet-stream",
            file,
            stored.size as u64,
        ))
    }

    fn authenticate(&self, request: &Request<'_>) -> Result<Worker, Reject> {
        let token = request
            .bearer_token()
            .ok_or_else(|| reject(401, "a session token is required"))?;
        self.db
            .authenticate_worker(&token_hash(token))?
            .ok_or_else(|| reject(401, "unknown session token"))
    }

    fn register(&self, request: &mut Request<'_>) -> Result<Response, Reject> {
        let enrolment = request
            .bearer_token()
            .ok_or_else(|| reject(401, "an enrolment token is required"))?
            .to_string();
        let body = read_json(request)?;
        if int_field(&body, "protocol_version")? != PROTOCOL_VERSION {
            return Err(Reject(
                400,
                format!("this coordinator speaks protocol version {PROTOCOL_VERSION}"),
            ));
        }
        let os = str_field(&body, "os")?;
        let arch = str_field(&body, "arch")?;
        if !is_label(os) || !is_label(arch) {
            return Err(reject(
                400,
                "\"os\" and \"arch\" must be short lower-case names",
            ));
        }
        let session = random_token(32);
        let worker = self
            .db
            .register_worker(&token_hash(&enrolment), &token_hash(&session), os, arch)?
            .ok_or_else(|| reject(401, "unknown enrolment token"))?;
        eprintln!(
            "worker {} registered: pool {}, {}/{}",
            worker.id, worker.pool, worker.os, worker.arch
        );
        Ok(json_ok(Value::object([
            ("worker_id", Value::Int(worker.id)),
            ("session_token", Value::from(session)),
            ("pool", Value::from(worker.pool)),
            (
                "claim_interval_seconds",
                Value::Int(self.settings.claim_interval_seconds),
            ),
        ])))
    }

    fn claim(&self, worker: &Worker) -> Result<Response, Reject> {
        // A draining worker only asks again after finishing what it had,
        // so this is the moment it is idle and can stop.
        if worker.draining {
            eprintln!("worker {} told to shut down: it is draining", worker.id);
            return Ok(json_ok(Value::object([
                ("job", Value::Null),
                ("shut_down", Value::Bool(true)),
            ])));
        }
        let Some(job) = self.db.claim_job(worker, self.settings.lease_seconds)? else {
            return Ok(json_ok(Value::object([("job", Value::Null)])));
        };
        // The stored description was validated when the job was
        // submitted, so it parses.
        let mut description = ci_json::parse(&job.description).map_err(|e| {
            eprintln!("error: job {} has an unreadable description: {e}", job.id);
            reject(500, "internal error")
        })?;
        // A stored build context is named, not pinned, in the stored
        // description. The worker is told its hash as of now, which is
        // also what names the image the recipe makes.
        if let Ok(parsed) = JobDescription::parse(&job.description) {
            if let Image::Build(build) = &parsed.image {
                if let Source::Stored(name) = &build.context {
                    if let Some(stored) = self.db.stored_file(name)? {
                        set_context_sha256(&mut description, &stored.sha256);
                    }
                }
            }
        }
        eprintln!(
            "job {} attempt {} claimed by worker {}",
            job.id, job.attempt, worker.id
        );
        Ok(json_ok(Value::object([(
            "job",
            Value::object([
                ("job_id", Value::Int(job.id)),
                ("attempt", Value::Int(job.attempt)),
                ("description", description),
                (
                    "report_interval_seconds",
                    Value::Int(self.settings.report_interval_seconds),
                ),
            ]),
        )])))
    }

    fn report(&self, worker: &Worker, request: &mut Request<'_>) -> Result<Response, Reject> {
        let body = read_json(request)?;
        let job_id = int_field(&body, "job_id")?;
        let attempt = int_field(&body, "attempt")?;
        let offset = int_field(&body, "log_offset")?;
        let chunk = match body.get("log_base64") {
            None => Vec::new(),
            Some(value) => value
                .as_str()
                .and_then(base64_decode)
                .ok_or_else(|| reject(400, "\"log_base64\" must be base64"))?,
        };
        if offset < 0 {
            return Err(reject(400, "\"log_offset\" must not be negative"));
        }

        let log_path = self.log_path(job_id, attempt);
        let result = self.db.with(|c| {
            c.transaction(|c| -> Result<(Action, i64), DbError> {
                let Some(job) = db::lock_job(c, job_id)? else {
                    return Ok((Action::Abandon, 0));
                };
                if !job.held_by(worker.id, attempt) {
                    return Ok((Action::Abandon, 0));
                }
                // The row lock makes this the only writer of the log.
                let stored = append_log(&log_path, job.log_bytes as u64, offset as u64, &chunk)
                    .map_err(DbError::Io)? as i64;
                db::renew_lease(c, job_id, stored, self.settings.lease_seconds)?;
                let action = if job.cancel_requested {
                    Action::Cancel
                } else {
                    Action::Continue
                };
                Ok((action, stored))
            })
        })?;
        let (action, stored) = result;
        Ok(json_ok(Value::object([
            ("action", Value::from(action.as_str())),
            // Where the next chunk should start. If a Report was lost,
            // this tells the worker what to send again.
            ("log_offset", Value::Int(stored)),
        ])))
    }

    fn finish(&self, worker: &Worker, request: &mut Request<'_>) -> Result<Response, Reject> {
        let body = read_json(request)?;
        let job_id = int_field(&body, "job_id")?;
        let attempt = int_field(&body, "attempt")?;
        let outcome = Outcome::parse(str_field(&body, "outcome")?)
            .ok_or_else(|| reject(400, "unknown \"outcome\""))?;
        let exit_code = match body.get("exit_code") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .ok_or_else(|| reject(400, "\"exit_code\" must be a whole number or null"))?,
            ),
        };
        let message = body.get("message").and_then(Value::as_str).unwrap_or("");
        let image_id = body.get("image_id").and_then(Value::as_str).unwrap_or("");
        let mut manifest = Vec::new();
        if let Some(list) = body.get("artifacts") {
            for item in list
                .as_array()
                .ok_or_else(|| reject(400, "\"artifacts\" must be an array"))?
            {
                manifest.push(Artifact {
                    path: str_field(item, "path")?.to_string(),
                    size: int_field(item, "size")?,
                    sha256: str_field(item, "sha256")?.to_string(),
                });
            }
        }

        enum Done {
            Acknowledged,
            Abandon,
            BadManifest(String),
        }
        let settings = &self.settings;
        let done = self.db.with(|c| {
            c.transaction(|c| -> Result<Done, DbError> {
                let Some(job) = db::lock_job(c, job_id)? else {
                    return Ok(Done::Abandon);
                };
                // A Finish that is sent again after its reply was lost
                // finds the job already finished by the same attempt.
                if job.state != "running"
                    && job.attempt == attempt
                    && job.worker_id == Some(worker.id)
                {
                    return Ok(Done::Acknowledged);
                }
                if !job.held_by(worker.id, attempt) {
                    return Ok(Done::Abandon);
                }
                // Every artifact the worker lists must have arrived, with
                // the size and hash the worker says it sent.
                let stored = db::artifacts_of(c, job_id, attempt)?;
                for claimed in &manifest {
                    match stored.iter().find(|a| a.path == claimed.path) {
                        Some(a)
                            if a.size == claimed.size
                                && a.sha256.eq_ignore_ascii_case(&claimed.sha256) => {}
                        Some(_) => {
                            return Ok(Done::BadManifest(format!(
                                "artifact {:?} does not match what was uploaded",
                                claimed.path
                            )))
                        }
                        None => {
                            return Ok(Done::BadManifest(format!(
                                "artifact {:?} was never uploaded",
                                claimed.path
                            )))
                        }
                    }
                }
                if outcome == Outcome::InfrastructureFailure {
                    let result = db::infrastructure_failure(
                        c,
                        job_id,
                        job.infrastructure_failures,
                        message,
                        image_id,
                        settings.max_infrastructure_failures,
                        settings.retry_backoff_seconds,
                    )?;
                    eprintln!(
                        "job {job_id} attempt {attempt}: infrastructure failure ({message}); {}",
                        match result {
                            Requeue::Requeued => "requeued",
                            Requeue::GaveUp => "giving up",
                        }
                    );
                } else {
                    db::finish_job(c, job_id, outcome, exit_code, message, image_id)?;
                    eprintln!("job {job_id} attempt {attempt}: {}", outcome.as_str());
                }
                Ok(Done::Acknowledged)
            })
        })?;
        match done {
            Done::Acknowledged => Ok(json_ok(Value::object([(
                "acknowledged",
                Value::Bool(true),
            )]))),
            Done::Abandon => Err(reject(409, "this job no longer belongs to this worker")),
            Done::BadManifest(message) => Err(Reject(400, message)),
        }
    }

    fn upload_artifact(
        &self,
        worker: &Worker,
        job_id: i64,
        attempt: i64,
        path: &str,
        request: &mut Request<'_>,
    ) -> Result<Response, Reject> {
        if !is_safe_artifact_path(path) {
            return Err(reject(400, "the artifact path is not allowed"));
        }
        if !self.db.job_held_by(job_id, worker.id, attempt)? {
            return Err(reject(409, "this job no longer belongs to this worker"));
        }
        if request
            .content_length
            .is_some_and(|len| len > self.settings.max_artifact_bytes)
        {
            return Err(reject(413, "the artifact is too large"));
        }
        let target = self.artifact_dir(job_id, attempt).join(path);
        let parent = target
            .parent()
            .ok_or_else(|| reject(400, "bad artifact path"))?;
        fs::create_dir_all(parent)?;
        // Written under a temporary name and renamed, so a half-received
        // file is never visible under its real name.
        let temporary = parent.join(format!(".upload-{}", random_token(8)));
        let received = receive_file(&temporary, request.body, self.settings.max_artifact_bytes);
        let (size, digest) = match received {
            Ok(Some(result)) => result,
            Ok(None) => {
                let _ = fs::remove_file(&temporary);
                return Err(reject(413, "the artifact is too large"));
            }
            Err(e) => {
                let _ = fs::remove_file(&temporary);
                // Most likely the worker's connection broke mid-upload.
                return Err(Reject(400, format!("the upload did not complete: {e}")));
            }
        };
        if request.content_length.is_some_and(|len| len != size) {
            let _ = fs::remove_file(&temporary);
            return Err(reject(400, "the upload did not complete"));
        }
        fs::rename(&temporary, &target)?;
        let artifact = Artifact {
            path: path.to_string(),
            size: size as i64,
            sha256: hex_encode(&digest),
        };
        self.db.record_artifact(job_id, attempt, &artifact)?;
        Ok(json_ok(Value::object([
            ("size", Value::Int(artifact.size)),
            ("sha256", Value::from(artifact.sha256)),
        ])))
    }

    // -----------------------------------------------------------------
    // Admin API
    // -----------------------------------------------------------------

    pub fn handle_admin(&self, request: &mut Request<'_>) -> Response {
        to_response(self.route_admin(request))
    }

    fn route_admin(&self, request: &mut Request<'_>) -> Result<Response, Reject> {
        // Checked before anything else, so that an unauthenticated caller
        // learns nothing, not even which paths exist.
        let token = request
            .bearer_token()
            .ok_or_else(|| reject(401, "an admin token is required"))?;
        let (_, admin) = self
            .db
            .authenticate_admin(&token_hash(token))?
            .ok_or_else(|| reject(401, "unknown admin token"))?;

        let path = request.path.clone();
        let segments = path_segments(&path);
        let job = |text: &str| text.parse::<i64>().map_err(|_| reject(404, "no such job"));
        let token_id = |text: &str| {
            text.parse::<i64>()
                .map_err(|_| reject(404, "no such token"))
        };
        match (request.method.as_str(), segments.as_slice()) {
            ("POST", ["admin", "v1", "admin-tokens"]) => self.create_admin_token(&admin, request),
            ("POST", ["admin", "v1", "admin-tokens", id, "revoke"]) => {
                let id = token_id(id)?;
                if !self.db.revoke_admin_token(id)? {
                    return Err(reject(404, "no such token"));
                }
                eprintln!("admin {admin:?}: revoked admin token {id}");
                Ok(json_ok(Value::object([("revoked", Value::Bool(true))])))
            }
            ("POST", ["admin", "v1", "worker-tokens"]) => self.create_worker_token(&admin, request),
            ("POST", ["admin", "v1", "worker-tokens", id, "revoke"]) => {
                let id = token_id(id)?;
                if !self.db.revoke_worker_token(id)? {
                    return Err(reject(404, "no such token"));
                }
                eprintln!("admin {admin:?}: revoked worker token {id}");
                Ok(json_ok(Value::object([("revoked", Value::Bool(true))])))
            }
            ("GET", ["admin", "v1", "workers"]) => self.list_workers(),
            ("POST", ["admin", "v1", "workers", id, "drain"]) => {
                let id = id
                    .parse::<i64>()
                    .map_err(|_| reject(404, "no such worker"))?;
                if !self.db.drain_worker(id)? {
                    return Err(reject(404, "no such worker"));
                }
                eprintln!("admin {admin:?}: draining worker {id}");
                Ok(json_ok(Value::object([("draining", Value::Bool(true))])))
            }
            ("GET", ["admin", "v1", "files"]) => self.list_stored_files(),
            ("PUT", ["admin", "v1", "files", name]) => self.put_stored_file(&admin, name, request),
            ("DELETE", ["admin", "v1", "files", name]) => {
                if !is_plain_file_name(name) || !self.db.delete_stored_file(name)? {
                    return Err(reject(404, "no such stored file"));
                }
                let _ = fs::remove_file(self.stored_file_path(name));
                eprintln!("admin {admin:?}: deleted stored file {name}");
                Ok(json_ok(Value::object([("deleted", Value::Bool(true))])))
            }
            ("POST", ["admin", "v1", "jobs"]) => self.submit_job(&admin, request),
            ("GET", ["admin", "v1", "jobs", id]) => self.job_status(job(id)?),
            ("POST", ["admin", "v1", "jobs", id, "cancel"]) => self.cancel_job(&admin, job(id)?),
            ("GET", ["admin", "v1", "jobs", id, "log"]) => self.job_log(job(id)?),
            ("GET", ["admin", "v1", "jobs", id, "artifacts", rest @ ..]) if !rest.is_empty() => {
                self.job_artifact(job(id)?, &rest.join("/"))
            }
            _ => Err(reject(404, "not found")),
        }
    }

    /// Creates the first admin token if there is none, and returns it.
    /// The caller writes it somewhere only an administrator can read.
    pub fn bootstrap_admin_token(&self) -> Result<Option<String>, DbError> {
        let token = random_token(32);
        let created = self.db.create_first_admin_token(
            "first admin token, created at start-up",
            &token_hash(&token),
        )?;
        Ok(created.then_some(token))
    }

    fn create_admin_token(
        &self,
        admin: &str,
        request: &mut Request<'_>,
    ) -> Result<Response, Reject> {
        let body = read_json(request)?;
        let name = str_field(&body, "name")?;
        if name.is_empty() || name.len() > 200 || name.chars().any(char::is_control) {
            return Err(reject(400, "\"name\" must be a short description"));
        }
        let token = random_token(32);
        let id = self.db.create_admin_token(name, &token_hash(&token))?;
        eprintln!("admin {admin:?}: created admin token {id}: {name}");
        Ok(Response::json(
            201,
            Value::object([
                ("token_id", Value::Int(id)),
                // Shown once. Only its hash is kept.
                ("token", Value::from(token)),
            ])
            .to_json(),
        ))
    }

    fn create_worker_token(
        &self,
        admin: &str,
        request: &mut Request<'_>,
    ) -> Result<Response, Reject> {
        let body = read_json(request)?;
        let name = str_field(&body, "name")?;
        let pool = str_field(&body, "pool")?;
        if name.is_empty() || name.len() > 200 || name.chars().any(char::is_control) {
            return Err(reject(400, "\"name\" must be a short description"));
        }
        if !is_label(pool) {
            return Err(reject(400, "\"pool\" must be a short lower-case name"));
        }
        let token = random_token(32);
        let id = self
            .db
            .create_worker_token(name, pool, &token_hash(&token))?;
        eprintln!("admin {admin:?}: created worker token {id} for pool {pool}: {name}");
        Ok(Response::json(
            201,
            Value::object([
                ("token_id", Value::Int(id)),
                // Shown once. Only its hash is kept.
                ("token", Value::from(token)),
            ])
            .to_json(),
        ))
    }

    fn list_workers(&self) -> Result<Response, Reject> {
        let workers = self.db.workers()?;
        Ok(json_ok(Value::object([(
            "workers",
            Value::Array(
                workers
                    .into_iter()
                    .map(|w| {
                        Value::object([
                            ("worker_id", Value::Int(w.id)),
                            ("token_id", Value::Int(w.token_id)),
                            ("token_name", Value::from(w.token_name)),
                            ("pool", Value::from(w.pool)),
                            ("os", Value::from(w.os)),
                            ("arch", Value::from(w.arch)),
                            ("registered_at", Value::from(w.registered_at)),
                            ("last_seen_at", Value::from(w.last_seen_at)),
                            ("draining", Value::Bool(w.draining)),
                            ("job_id", w.job_id.map_or(Value::Null, Value::Int)),
                        ])
                    })
                    .collect(),
            ),
        )])))
    }

    fn list_stored_files(&self) -> Result<Response, Reject> {
        let files = self.db.stored_files()?;
        Ok(json_ok(Value::object([(
            "files",
            Value::Array(
                files
                    .into_iter()
                    .map(|f| {
                        Value::object([
                            ("name", Value::from(f.path)),
                            ("size", Value::Int(f.size)),
                            ("sha256", Value::from(f.sha256)),
                        ])
                    })
                    .collect(),
            ),
        )])))
    }

    /// Stores a file under a name, replacing any earlier one. Jobs that
    /// name it read whatever is stored when they run.
    fn put_stored_file(
        &self,
        admin: &str,
        name: &str,
        request: &mut Request<'_>,
    ) -> Result<Response, Reject> {
        if !is_plain_file_name(name) {
            return Err(reject(400, "the file name must be a plain file name"));
        }
        if request
            .content_length
            .is_some_and(|len| len > self.settings.max_artifact_bytes)
        {
            return Err(reject(413, "the file is too large"));
        }
        let target = self.stored_file_path(name);
        let parent = target
            .parent()
            .ok_or_else(|| reject(400, "bad file name"))?;
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".upload-{}", random_token(8)));
        let (size, digest) =
            match receive_file(&temporary, request.body, self.settings.max_artifact_bytes) {
                Ok(Some(result)) => result,
                Ok(None) => {
                    let _ = fs::remove_file(&temporary);
                    return Err(reject(413, "the file is too large"));
                }
                Err(e) => {
                    let _ = fs::remove_file(&temporary);
                    return Err(Reject(400, format!("the upload did not complete: {e}")));
                }
            };
        if request.content_length.is_some_and(|len| len != size) {
            let _ = fs::remove_file(&temporary);
            return Err(reject(400, "the upload did not complete"));
        }
        fs::rename(&temporary, &target)?;
        let stored = Artifact {
            path: name.to_string(),
            size: size as i64,
            sha256: hex_encode(&digest),
        };
        self.db.put_stored_file(&stored)?;
        eprintln!(
            "admin {admin:?}: stored file {name} ({size} bytes, sha256 {})",
            stored.sha256
        );
        Ok(json_ok(Value::object([
            ("name", Value::from(name)),
            ("size", Value::Int(stored.size)),
            ("sha256", Value::from(stored.sha256)),
        ])))
    }

    fn submit_job(&self, admin: &str, request: &mut Request<'_>) -> Result<Response, Reject> {
        let body = read_json(request)?;
        let pool = str_field(&body, "pool")?;
        let os = str_field(&body, "os")?;
        let arch = str_field(&body, "arch")?;
        if !is_label(pool) || !is_label(os) || !is_label(arch) {
            return Err(reject(
                400,
                "\"pool\", \"os\" and \"arch\" must be short lower-case names",
            ));
        }
        let description = body
            .get("description")
            .ok_or_else(|| reject(400, "\"description\" is required"))?
            .to_json();
        let parsed = JobDescription::parse(&description)
            .map_err(|e| Reject(400, format!("job description: {e}")))?;
        if parsed.has_local_inputs() {
            // A file:// input would make a worker read whatever path the
            // submitter names from its own disk.
            return Err(reject(
                400,
                "job description: files must come from http://, https:// or stored: URLs",
            ));
        }
        // A job that names a file nobody has stored could never run.
        for name in parsed.stored_files() {
            if self.db.stored_file(name)?.is_none() {
                return Err(Reject(
                    400,
                    format!("job description: there is no stored file named {name:?}"),
                ));
            }
        }
        let id = self.db.submit_job(pool, os, arch, &description)?;
        eprintln!("admin {admin:?}: submitted job {id}: pool {pool}, {os}/{arch}");
        Ok(Response::json(
            201,
            Value::object([("job_id", Value::Int(id))]).to_json(),
        ))
    }

    fn job_status(&self, job_id: i64) -> Result<Response, Reject> {
        let job = self
            .db
            .job_status(job_id)?
            .ok_or_else(|| reject(404, "no such job"))?;
        let optional = |value: Option<String>| value.map(Value::String).unwrap_or(Value::Null);
        Ok(json_ok(Value::object([
            ("job_id", Value::Int(job.id)),
            ("pool", Value::from(job.pool)),
            ("os", Value::from(job.os)),
            ("arch", Value::from(job.arch)),
            ("state", Value::from(job.state)),
            ("outcome", optional(job.outcome)),
            (
                "exit_code",
                job.exit_code.map(Value::Int).unwrap_or(Value::Null),
            ),
            ("message", Value::from(job.message)),
            ("image_id", Value::from(job.image_id)),
            ("attempt", Value::Int(job.attempt)),
            (
                "infrastructure_failures",
                Value::Int(job.infrastructure_failures),
            ),
            ("cancel_requested", Value::Bool(job.cancel_requested)),
            ("log_bytes", Value::Int(job.log_bytes)),
            ("created_at", Value::from(job.created_at)),
            ("started_at", optional(job.started_at)),
            ("finished_at", optional(job.finished_at)),
            (
                "artifacts",
                Value::Array(
                    job.artifacts
                        .into_iter()
                        .map(|a| {
                            Value::object([
                                ("path", Value::from(a.path)),
                                ("size", Value::Int(a.size)),
                                ("sha256", Value::from(a.sha256)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])))
    }

    fn cancel_job(&self, admin: &str, job_id: i64) -> Result<Response, Reject> {
        if !self.db.cancel_job(job_id)? {
            return Err(reject(404, "no such job"));
        }
        eprintln!("admin {admin:?}: requested cancellation of job {job_id}");
        self.job_status(job_id)
    }

    /// The log of the job's latest attempt, up to what has been stored.
    fn job_log(&self, job_id: i64) -> Result<Response, Reject> {
        let job = self
            .db
            .job_status(job_id)?
            .ok_or_else(|| reject(404, "no such job"))?;
        if job.log_bytes == 0 {
            return Ok(Response::text(200, Vec::new()));
        }
        let file = File::open(self.log_path(job_id, job.attempt))?;
        Ok(Response::file(
            "text/plain; charset=utf-8",
            file,
            job.log_bytes as u64,
        ))
    }

    fn job_artifact(&self, job_id: i64, path: &str) -> Result<Response, Reject> {
        if !is_safe_artifact_path(path) {
            return Err(reject(404, "no such artifact"));
        }
        let job = self
            .db
            .job_status(job_id)?
            .ok_or_else(|| reject(404, "no such job"))?;
        // Only what the database lists is served, at the size recorded.
        let artifact = job
            .artifacts
            .iter()
            .find(|a| a.path == path)
            .ok_or_else(|| reject(404, "no such artifact"))?;
        let file = File::open(self.artifact_dir(job_id, job.attempt).join(path))?;
        Ok(Response::file(
            "application/octet-stream",
            file,
            artifact.size as u64,
        ))
    }

    // -----------------------------------------------------------------
    // Storage layout
    // -----------------------------------------------------------------

    fn stored_file_path(&self, name: &str) -> PathBuf {
        self.settings.data_dir.join("files").join(name)
    }

    fn attempt_dir(&self, job_id: i64, attempt: i64) -> PathBuf {
        self.settings
            .data_dir
            .join("jobs")
            .join(job_id.to_string())
            .join(attempt.to_string())
    }

    fn log_path(&self, job_id: i64, attempt: i64) -> PathBuf {
        self.attempt_dir(job_id, attempt).join("log.txt")
    }

    fn artifact_dir(&self, job_id: i64, attempt: i64) -> PathBuf {
        self.attempt_dir(job_id, attempt).join("artifacts")
    }
}

/// Adds a chunk of log that the worker says starts at `offset`, when
/// `stored` bytes are already recorded. Returns the new recorded length.
///
/// - A chunk that starts where the log ends is appended.
/// - A chunk that overlaps what is stored was sent before; only the new
///   part, if any, is appended.
/// - A chunk that starts past the end means a Report was lost. Nothing is
///   stored, and the unchanged length tells the worker where to resume.
fn append_log(path: &Path, stored: u64, offset: u64, chunk: &[u8]) -> io::Result<u64> {
    if offset > stored {
        return Ok(stored);
    }
    let already = (stored - offset).min(chunk.len() as u64) as usize;
    let fresh = &chunk[already..];
    if fresh.is_empty() {
        return Ok(stored);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    // If an earlier Report wrote to the file but its transaction did not
    // commit, the file is longer than the recorded length. The recorded
    // length is the truth.
    file.set_len(stored)?;
    file.seek(SeekFrom::Start(stored))?;
    file.write_all(fresh)?;
    file.flush()?;
    Ok(stored + fresh.len() as u64)
}

/// Streams a request body to a file, hashing it on the way. Returns
/// `None` if it is larger than `limit`.
fn receive_file(
    path: &Path,
    body: &mut dyn Read,
    limit: u64,
) -> io::Result<Option<(u64, [u8; 32])>> {
    let mut file = File::create(path)?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = match body.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        size += n as u64;
        if size > limit {
            return Ok(None);
        }
        hash.update(&buffer[..n]);
        file.write_all(&buffer[..n])?;
    }
    file.sync_all()?;
    Ok(Some((size, hash.finish())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ci_coordinator_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn log_chunks_are_appended_once() {
        let path = scratch("log").join("deep/log.txt");
        // In order.
        assert_eq!(append_log(&path, 0, 0, b"hello ").unwrap(), 6);
        assert_eq!(append_log(&path, 6, 6, b"world").unwrap(), 11);
        // The same chunk again: nothing changes.
        assert_eq!(append_log(&path, 11, 6, b"world").unwrap(), 11);
        // A chunk that overlaps: only the new part is added.
        assert_eq!(append_log(&path, 11, 9, b"ld, again").unwrap(), 18);
        // A chunk from the future: refused, length unchanged.
        assert_eq!(append_log(&path, 18, 30, b"lost").unwrap(), 18);
        // An empty chunk is a heartbeat.
        assert_eq!(append_log(&path, 18, 18, b"").unwrap(), 18);
        assert_eq!(fs::read(&path).unwrap(), b"hello world, again");
    }

    #[test]
    fn log_file_longer_than_recorded_is_cut_back() {
        let path = scratch("log_cut").join("log.txt");
        fs::write(&path, b"committed|uncommitted tail").unwrap();
        assert_eq!(append_log(&path, 10, 10, b"next").unwrap(), 14);
        assert_eq!(fs::read(&path).unwrap(), b"committed|next");
    }

    #[test]
    fn received_files_are_hashed_and_limited() {
        let dir = scratch("receive");
        let (size, digest) = receive_file(&dir.join("a"), &mut &b"abc"[..], 10)
            .unwrap()
            .unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            hex_encode(&digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(receive_file(&dir.join("b"), &mut &b"0123456789x"[..], 10)
            .unwrap()
            .is_none());
        assert!(receive_file(&dir.join("c"), &mut &b"0123456789"[..], 10)
            .unwrap()
            .is_some());
    }

    #[test]
    fn labels() {
        for good in ["release", "pull-request", "linux", "amd64", "arm_64"] {
            assert!(is_label(good), "{good}");
        }
        for bad in ["", "Release", "a b", "a/b", "a.b", "é", &"x".repeat(65)] {
            assert!(!is_label(bad), "{bad:?}");
        }
    }

    #[test]
    fn token_hashes_are_sha256_hex() {
        assert_eq!(
            token_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
