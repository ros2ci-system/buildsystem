// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The source archive: every release, kept as the tarball a job made of
//! it, named by the tarball's hash.
//!
//! A release is archived once. The entry for a version of a repository
//! is never replaced: if upstream later moves the tag, the archive still
//! holds what was released, and builds go on reading that. Package
//! builds name the tarball by its hash as an `archive:` input.

use std::fs::{self, File};
use std::path::PathBuf;

use ci_http::server::{Request, Response};
use ci_json::Value;
use ci_protocol::{JobDescription, Outcome};

use super::repositories::place;
use super::{int_field, is_label, json_ok, read_json, reject, str_field, Coordinator, Reject};
use crate::db::{ArchivedRelease, Worker};

/// What a source archive job calls the tarball and its description.
const TARBALL_ARTIFACT: &str = "source.tar.gz";
const RECORD_ARTIFACT: &str = "record.json";

/// Largest description read.
const MAX_RECORD: u64 = 16 * 1024 * 1024;

/// A ROS package or repository name.
fn is_ros_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_version(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text.starts_with(|c: char| c.is_ascii_digit())
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".+~".contains(c))
}

/// A git commit id: 40 or 64 lower-case hex digits.
fn is_commit(text: &str) -> bool {
    matches!(text.len(), 40 | 64)
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && is_commit(text)
}

/// Text that goes into records and logs: one line, no control characters.
fn is_plain_text(text: &str) -> bool {
    !text.is_empty() && text.len() <= 512 && !text.chars().any(char::is_control)
}

fn release_json(release: &ArchivedRelease) -> Value {
    // The record was written by this coordinator as JSON.
    ci_json::parse(&release.record).unwrap_or(Value::Null)
}

impl Coordinator {
    fn archive_path(&self, sha256: &str) -> PathBuf {
        self.settings
            .data_dir
            .join("archive")
            .join(format!("{sha256}.tar.gz"))
    }

    /// Takes the tarball a source archive job made into the archive.
    ///
    /// The caller states which release it is and which commit it must
    /// be. The job's own description has to agree, and the tarball is
    /// named by the hash this coordinator computed when it was uploaded.
    pub(super) fn archive_release(
        &self,
        admin: &str,
        request: &mut Request<'_>,
    ) -> Result<Response, Reject> {
        let body = read_json(request)?;
        let job_id = int_field(&body, "job_id")?;
        let rosdistro = str_field(&body, "rosdistro")?;
        let repository = str_field(&body, "repository")?;
        let version = str_field(&body, "version")?;
        let url = str_field(&body, "url")?;
        let tag = str_field(&body, "tag")?;
        let commit = str_field(&body, "commit")?;
        if !is_label(rosdistro)
            || !is_ros_name(repository)
            || !is_version(version)
            || !is_plain_text(url)
            || !is_plain_text(tag)
            || !is_commit(commit)
        {
            return Err(reject(400, "the release is not described properly"));
        }

        let job = self
            .db
            .job_status(job_id)?
            .ok_or_else(|| reject(404, "no such job"))?;
        if job.state != "finished" || job.outcome.as_deref() != Some(Outcome::Success.as_str()) {
            return Err(Reject(
                409,
                format!("job {job_id} has not succeeded, so there is nothing to archive"),
            ));
        }
        let artifact = |path: &str| {
            job.artifacts
                .iter()
                .find(|a| a.path == path)
                .ok_or_else(|| Reject(400, format!("job {job_id} has no {path}")))
        };
        let tarball = artifact(TARBALL_ARTIFACT)?;
        if artifact(RECORD_ARTIFACT)?.size as u64 > MAX_RECORD {
            return Err(reject(
                400,
                "the job's description of the release is too large",
            ));
        }
        let artifact_dir = self.artifact_dir(job_id, job.attempt);
        let described = ci_json::parse_bytes(&fs::read(artifact_dir.join(RECORD_ARTIFACT))?)
            .map_err(|e| Reject(400, format!("{RECORD_ARTIFACT}: {e}")))?;
        let stated = |name: &str| described.get(name).and_then(Value::as_str).unwrap_or("");
        for (name, expected) in [
            ("repository", repository),
            ("version", version),
            ("url", url),
            ("tag", tag),
            ("commit", commit),
        ] {
            if stated(name) != expected {
                return Err(Reject(
                    400,
                    format!(
                        "job {job_id} archived {name} {:?}, not {expected:?}",
                        stated(name)
                    ),
                ));
            }
        }
        if !stated("sha256").eq_ignore_ascii_case(&tarball.sha256) {
            return Err(Reject(
                400,
                format!("job {job_id} describes a tarball other than the one it uploaded"),
            ));
        }

        // The packages in the release, as the job found them at the tag.
        // Every one must carry the release's version.
        let mut packages = Vec::new();
        for package in described
            .get("packages")
            .and_then(Value::as_array)
            .unwrap_or_default()
        {
            let field = |name: &str| package.get(name).and_then(Value::as_str).unwrap_or("");
            let (name, path, build_type) = (field("name"), field("path"), field("build_type"));
            if !is_ros_name(name) || !is_plain_text(path) || !is_plain_text(build_type) {
                return Err(reject(
                    400,
                    "a package of the release is not described properly",
                ));
            }
            if field("version") != version {
                return Err(Reject(
                    400,
                    format!(
                        "package {name} has version {:?}, and the release is {version}",
                        field("version")
                    ),
                ));
            }
            packages.push(Value::object([
                ("name", Value::from(name)),
                ("path", Value::from(path)),
                ("build_type", Value::from(build_type)),
            ]));
        }
        if packages.is_empty() {
            return Err(reject(400, "the release holds no packages"));
        }
        let skipped: Vec<&str> = described
            .get("skipped")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| is_ros_name(name))
            .collect();

        let _guard = self.publishing.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = self.db.archived_release(rosdistro, repository, version)? {
            // Archiving the same commit again gives the same tarball and
            // changes nothing. Anything else is refused: what was
            // released stays what was released.
            if existing.commit == commit && existing.sha256 == tarball.sha256 {
                return Ok(json_ok(release_json(&existing)));
            }
            return Err(Reject(
                409,
                format!(
                    "{repository} {version} is already archived, from commit {} as {}",
                    existing.commit, existing.sha256
                ),
            ));
        }
        place(
            &artifact_dir.join(TARBALL_ARTIFACT),
            &self.archive_path(&tarball.sha256),
        )?;
        let release = ArchivedRelease {
            rosdistro: rosdistro.to_string(),
            repository: repository.to_string(),
            version: version.to_string(),
            commit: commit.to_string(),
            sha256: tarball.sha256.clone(),
            record: Value::object([
                ("rosdistro", Value::from(rosdistro)),
                ("repository", Value::from(repository)),
                ("version", Value::from(version)),
                ("url", Value::from(url)),
                ("tag", Value::from(tag)),
                ("commit", Value::from(commit)),
                ("sha256", Value::from(tarball.sha256.as_str())),
                ("size", Value::Int(tarball.size)),
                ("packages", Value::Array(packages)),
                ("skipped", Value::strings(skipped)),
                (
                    "rosdistro_url",
                    Value::from(
                        Some(stated("rosdistro_url"))
                            .filter(|text| is_plain_text(text))
                            .unwrap_or(""),
                    ),
                ),
                ("job_id", Value::Int(job_id)),
            ])
            .to_json(),
        };
        self.db.archive_release(&release, job_id, job.attempt)?;
        eprintln!(
            "admin {admin:?}: archived {repository} {version} of {rosdistro} from commit {commit} as {}",
            tarball.sha256
        );
        Ok(Response::json(201, release.record))
    }

    /// Every archived release of a ROS distribution.
    pub(super) fn archived_releases(&self, rosdistro: &str) -> Result<Response, Reject> {
        if !is_label(rosdistro) {
            return Err(reject(404, "no such distribution"));
        }
        let releases = self.db.archived_releases(rosdistro)?;
        Ok(json_ok(Value::object([(
            "releases",
            Value::Array(releases.iter().map(release_json).collect()),
        )])))
    }

    /// Whether a tarball of this hash is in the archive.
    pub(super) fn is_archived(&self, sha256: &str) -> Result<bool, Reject> {
        Ok(self.db.archived_tarball(sha256)?)
    }

    /// Serves an archived tarball to the worker running a job that
    /// names it as an input.
    pub(super) fn download_archived(
        &self,
        worker: &Worker,
        job_id: i64,
        attempt: i64,
        sha256: &str,
    ) -> Result<Response, Reject> {
        let description = self
            .db
            .held_job_description(job_id, worker.id, attempt)?
            .ok_or_else(|| reject(409, "this job no longer belongs to this worker"))?;
        let job = JobDescription::parse(&description).map_err(|e| {
            eprintln!("error: job {job_id} has an unreadable description: {e}");
            reject(500, "internal error")
        })?;
        if !is_sha256(sha256) || !job.archived_sources().contains(&sha256) {
            return Err(reject(404, "this job does not read that tarball"));
        }
        let file = File::open(self.archive_path(sha256))?;
        let len = file.metadata()?.len();
        Ok(Response::file("application/octet-stream", file, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_versions_and_commits() {
        assert!(is_ros_name("ament_cmake") && is_ros_name("rmw-x"));
        assert!(!is_ros_name("a/b") && !is_ros_name(""));
        assert!(is_version("0.19.1") && !is_version("v1") && !is_version("1-1"));
        assert!(is_commit(&"a".repeat(40)) && is_commit(&"0".repeat(64)));
        assert!(!is_commit(&"A".repeat(40)) && !is_commit("abc"));
        assert!(is_sha256(&"a".repeat(64)) && !is_sha256(&"a".repeat(40)));
        assert!(is_plain_text("https://example.invalid/x.git") && !is_plain_text("a\nb"));
    }
}
