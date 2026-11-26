// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Package repositories in the admin API, and the read-only listener
//! that serves them.
//!
//! Publishing is a separate step from building: a job that succeeded is
//! named in a publish call, with what it was supposed to build. The
//! check of what the job actually built against that, and the writing
//! of the index, are in `crate::repo`.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ci_http::server::{path_segments, Request, Response};
use ci_json::Value;
use ci_openpgp::SigningKey;
use ci_protocol::Outcome;

use super::{int_field, is_label, json_ok, read_json, reject, str_field, Coordinator, Reject};
use crate::repo::{self, Build, Format, Repository};

/// What a job calls its description of the packages it built.
const METADATA_ARTIFACT: &str = "repo-metadata.json";

/// Largest description read. The file lists of a large package are long.
const MAX_METADATA: u64 = 256 * 1024 * 1024;

/// A ROS package name, which also names a directory in the repository.
fn is_package_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// An upstream version, or the suffix that follows the rebuild counter.
fn is_version_text(text: &str) -> bool {
    text.len() <= 64
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".+~".contains(c))
}

/// The name of a binary or source package in either format.
fn is_binary_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-._".contains(c))
}

fn seconds_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Puts a job's artifact at a path in a repository. A hard link where
/// the two are on one filesystem, which costs no space; a copy otherwise.
fn place(source: &Path, target: &Path) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let temporary = target.with_file_name(format!(".placing-{name}"));
    let _ = fs::remove_file(&temporary);
    if fs::hard_link(source, &temporary).is_err() {
        fs::copy(source, &temporary)?;
    }
    fs::rename(&temporary, target)?;
    // Renaming one name of a file onto another name of the same file
    // does nothing and leaves both. That is the case when a build is
    // published a second time.
    let _ = fs::remove_file(&temporary);
    Ok(())
}

impl Coordinator {
    fn signing_key(&self) -> Result<&SigningKey, Reject> {
        self.signing_key.as_ref().ok_or_else(|| {
            reject(
                409,
                "the coordinator was started without --signing-key, so it cannot publish",
            )
        })
    }

    fn repository_root(&self, name: &str) -> PathBuf {
        self.settings.data_dir.join("repos").join(name)
    }

    fn load_repository(&self, name: &str) -> Result<Repository, Reject> {
        if !is_label(name) {
            return Err(reject(404, "no such repository"));
        }
        self.db
            .repository(name)?
            .ok_or_else(|| reject(404, "no such repository"))
    }

    /// Writes a repository's whole index from the database and signs it.
    fn write_repository(&self, repository: &Repository) -> Result<(), Reject> {
        let files = self.db.repository_files(&repository.name)?;
        repo::write_index(
            &self.repository_root(&repository.name),
            repository,
            &files,
            self.signing_key()?,
            seconds_now(),
        )?;
        Ok(())
    }

    pub(super) fn create_repository(
        &self,
        admin: &str,
        request: &mut Request<'_>,
    ) -> Result<Response, Reject> {
        self.signing_key()?;
        let body = read_json(request)?;
        let name = str_field(&body, "name")?;
        let format = Format::parse(str_field(&body, "format")?)
            .ok_or_else(|| reject(400, "\"format\" must be \"apt\" or \"rpm\""))?;
        let suite = body.get("suite").and_then(Value::as_str).unwrap_or("");
        let version_suffix = str_field(&body, "version_suffix")?;
        let architectures: Vec<String> = body
            .get("architectures")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let source_arch = str_field(&body, "source_arch")?;
        if !is_label(name) {
            return Err(reject(400, "\"name\" must be a short lower-case name"));
        }
        match format {
            Format::Apt if !is_label(suite) => {
                return Err(reject(400, "an apt repository needs a \"suite\""))
            }
            Format::Rpm if !suite.is_empty() => {
                return Err(reject(400, "an RPM repository has no \"suite\""))
            }
            _ => {}
        }
        if version_suffix.is_empty() || !is_version_text(version_suffix) {
            return Err(reject(400, "\"version_suffix\" is not usable in a version"));
        }
        if architectures.is_empty()
            || architectures
                .iter()
                .any(|arch| repo::format_arch(format, arch).is_none())
        {
            return Err(reject(
                400,
                "\"architectures\" must list one or more of amd64 and arm64",
            ));
        }
        if !architectures.iter().any(|arch| arch == source_arch) {
            return Err(reject(
                400,
                "\"source_arch\" must be one of the architectures",
            ));
        }
        let repository = Repository {
            name: name.to_string(),
            format,
            suite: suite.to_string(),
            version_suffix: version_suffix.to_string(),
            architectures,
            source_arch: source_arch.to_string(),
        };
        let _guard = self.publishing.lock().unwrap_or_else(|e| e.into_inner());
        if !self.db.create_repository(&repository)? {
            return Err(reject(409, "a repository of that name exists"));
        }
        // An empty repository is a valid one: clients can use it at once.
        self.write_repository(&repository)?;
        eprintln!(
            "admin {admin:?}: created {} repository {name}",
            format.as_str()
        );
        Ok(Response::json(
            201,
            Value::object([("name", Value::from(name))]).to_json(),
        ))
    }

    pub(super) fn repository_status(&self, name: &str) -> Result<Response, Reject> {
        let repository = self.load_repository(name)?;
        let builds = self.db.repository_builds(name)?;
        let key = self.signing_key.as_ref();
        Ok(json_ok(Value::object([
            ("name", Value::from(repository.name)),
            ("format", Value::from(repository.format.as_str())),
            ("suite", Value::from(repository.suite)),
            ("version_suffix", Value::from(repository.version_suffix)),
            ("architectures", Value::strings(&repository.architectures)),
            ("source_arch", Value::from(repository.source_arch)),
            (
                "key_fingerprint",
                key.map_or(Value::Null, |k| Value::from(k.fingerprint_hex())),
            ),
            (
                "public_key",
                key.map_or(Value::Null, |k| Value::from(k.public_key_armored())),
            ),
            (
                "builds",
                Value::Array(
                    builds
                        .into_iter()
                        .map(|b| {
                            Value::object([
                                ("package", Value::from(b.package)),
                                ("arch", Value::from(b.arch)),
                                ("version", Value::from(b.version)),
                                ("revision", Value::Int(b.revision)),
                                ("job_id", Value::Int(b.job_id)),
                                ("attempt", Value::Int(b.attempt)),
                                ("published_at", Value::from(b.published_at)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])))
    }

    /// Hands out the rebuild counter for the next build of a version of
    /// a package. The submitter gives it to the build as its revision.
    pub(super) fn allocate_revision(
        &self,
        admin: &str,
        name: &str,
        request: &mut Request<'_>,
    ) -> Result<Response, Reject> {
        let repository = self.load_repository(name)?;
        let body = read_json(request)?;
        let package = str_field(&body, "package")?;
        let version = str_field(&body, "version")?;
        if !is_package_name(package) || version.is_empty() || !is_version_text(version) {
            return Err(reject(400, "\"package\" or \"version\" is malformed"));
        }
        let revision = self.db.allocate_revision(name, package, version)?;
        eprintln!("admin {admin:?}: {name}: {package} {version} gets revision {revision}");
        Ok(json_ok(Value::object([
            ("revision", Value::Int(revision)),
            (
                "package_version",
                Value::from(format!("{version}-{revision}{}", repository.version_suffix)),
            ),
        ])))
    }

    /// Publishes the packages a finished job built.
    pub(super) fn publish(
        &self,
        admin: &str,
        name: &str,
        request: &mut Request<'_>,
    ) -> Result<Response, Reject> {
        let key = self.signing_key()?;
        let repository = self.load_repository(name)?;
        let body = read_json(request)?;
        let job_id = int_field(&body, "job_id")?;
        let package = str_field(&body, "package")?;
        let version = str_field(&body, "version")?;
        let revision = int_field(&body, "revision")?;
        let names: Vec<String> = body
            .get("names")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !is_package_name(package) || version.is_empty() || !is_version_text(version) {
            return Err(reject(400, "\"package\" or \"version\" is malformed"));
        }
        if names.is_empty() || !names.iter().all(|n| is_binary_name(n)) {
            return Err(reject(
                400,
                "\"names\" must list the package names this build may publish",
            ));
        }

        let job = self
            .db
            .job_status(job_id)?
            .ok_or_else(|| reject(404, "no such job"))?;
        if job.state != "finished" || job.outcome.as_deref() != Some(Outcome::Success.as_str()) {
            return Err(Reject(
                409,
                format!("job {job_id} has not succeeded, so there is nothing to publish"),
            ));
        }
        if !repository.architectures.contains(&job.arch) {
            return Err(Reject(
                400,
                format!("this repository holds nothing for {}", job.arch),
            ));
        }

        let _guard = self.publishing.lock().unwrap_or_else(|e| e.into_inner());

        // The rebuild counter must be one this repository handed out.
        let allocated = self.db.allocated_revision(name, package, version)?;
        if revision < 1 || allocated.is_none_or(|highest| revision > highest) {
            return Err(Reject(
                409,
                format!("revision {revision} of {package} {version} was never handed out"),
            ));
        }

        let artifact_dir = self.artifact_dir(job_id, job.attempt);
        let described = job
            .artifacts
            .iter()
            .find(|a| a.path == METADATA_ARTIFACT)
            .ok_or_else(|| {
                Reject(
                    400,
                    format!(
                        "job {job_id} has no {METADATA_ARTIFACT}, so it did not build packages"
                    ),
                )
            })?;
        if described.size as u64 > MAX_METADATA {
            return Err(reject(
                400,
                "the job's description of its packages is too large",
            ));
        }
        let metadata = ci_json::parse_bytes(&fs::read(artifact_dir.join(METADATA_ARTIFACT))?)
            .map_err(|e| Reject(400, format!("{METADATA_ARTIFACT}: {e}")))?;
        let planned = repo::plan(
            &Build {
                repository: &repository,
                package,
                arch: &job.arch,
                names: &names,
                version: &format!("{version}-{revision}{}", repository.version_suffix),
                artifacts: &job.artifacts,
            },
            &metadata,
        )
        .map_err(|message| Reject(400, format!("job {job_id}: {message}")))?;

        // A rebuild of the same version must carry a higher counter than
        // what is published, so that clients take it as an upgrade.
        let published = self
            .db
            .repository_builds(name)?
            .into_iter()
            .find(|b| b.package == package && b.arch == job.arch);
        if let Some(published) = &published {
            let same_build = published.job_id == job_id && published.attempt == job.attempt;
            if published.version == version && published.revision >= revision && !same_build {
                return Err(Reject(
                    409,
                    format!(
                        "revision {} of {package} {version} is published for {}, and {revision} is not newer",
                        published.revision, job.arch
                    ),
                ));
            }
        }

        // Files first, then the database, then the index: a file that
        // nothing lists yet is harmless, and an index never lists a file
        // that is not there.
        let root = self.repository_root(name);
        for file in &planned {
            place(&artifact_dir.join(&file.artifact), &root.join(&file.path))?;
        }
        let replaced = self.db.replace_repository_build(
            name,
            package,
            &job.arch,
            version,
            revision,
            job_id,
            job.attempt,
            &planned,
        )?;
        let files = self.db.repository_files(name)?;
        repo::write_index(&root, &repository, &files, key, seconds_now())?;
        for path in replaced {
            if !planned.iter().any(|file| file.path == path) {
                let _ = fs::remove_file(root.join(path));
            }
        }
        eprintln!(
            "admin {admin:?}: {name}: published {package} {version} revision {revision} for {} from job {job_id}",
            job.arch
        );
        Ok(json_ok(Value::object([
            ("package", Value::from(package)),
            ("arch", Value::from(job.arch)),
            (
                "files",
                Value::strings(planned.iter().map(|file| file.path.as_str())),
            ),
        ])))
    }

    /// Serves the repositories, read-only, to anyone. Everything under
    /// the repository directory is public by nature.
    pub fn handle_repository(&self, request: &mut Request<'_>) -> Response {
        if request.method != "GET" {
            return Response::text(405, "only GET is supported\n");
        }
        let segments = path_segments(&request.path);
        // Nothing hidden, nothing above the root, and no name that means
        // something else to a filesystem.
        let plain = !segments.is_empty()
            && segments.iter().all(|segment| {
                !segment.starts_with('.')
                    && !segment.contains(['\\', '\0', ':'])
                    && segment.len() <= 255
            });
        if plain {
            let path = segments
                .iter()
                .fold(self.settings.data_dir.join("repos"), |path, segment| {
                    path.join(segment)
                });
            if let Ok(file) = File::open(&path) {
                if let Ok(metadata) = file.metadata() {
                    if metadata.is_file() {
                        return Response::file("application/octet-stream", file, metadata.len());
                    }
                }
            }
        }
        Response::text(404, "not found\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_versions() {
        assert!(is_package_name("ament_cmake_core"));
        assert!(
            !is_package_name("ament-cmake") && !is_package_name("../x") && !is_package_name("")
        );
        assert!(
            is_version_text("0.19.1") && is_version_text(".el10") && is_version_text("resolute")
        );
        assert!(!is_version_text("1-2") && !is_version_text("1 2") && !is_version_text("1/2"));
        assert!(is_binary_name("ros-rolling-ament-package-runtime"));
        assert!(!is_binary_name("a b") && !is_binary_name("a/b"));
    }

    #[test]
    fn placed_files_replace_what_was_there() {
        let dir = std::env::temp_dir().join(format!("ros2ci-place-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a"), "one").unwrap();
        fs::write(dir.join("b"), "two").unwrap();
        let target = dir.join("pool/x/file.deb");
        place(&dir.join("a"), &target).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "one");
        place(&dir.join("b"), &target).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "two");
        assert_eq!(fs::read_to_string(dir.join("a")).unwrap(), "one");
        // The same file a second time leaves nothing behind.
        place(&dir.join("b"), &target).unwrap();
        let names: Vec<_> = fs::read_dir(dir.join("pool/x"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["file.deb"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
