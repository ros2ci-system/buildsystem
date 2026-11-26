// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The reconciler: compares what the rosdistro says should exist with
//! what is published, and queues what is missing.
//!
//! It works in steps, and each step reads its whole situation from the
//! database, does at most a little, and writes down what it did. Nothing
//! is remembered in between, so a restart in the middle of anything
//! picks up where the database says things are.
//!
//! **Intake.** It asks the rosdistro's repository which commit its
//! branch is at. For a new commit it runs the release check as a job,
//! and then a source archive job for each release that is new. Until
//! that is all done, builds go on from the last commit that was taken
//! in whole.
//!
//! **Builds.** For each target, each package is in one of the states of
//! the design. A package is up to date when what is published was built
//! from the release's source against the builds of its dependencies
//! that are published now. That one rule gives both the order and the
//! rebuilds: a package waits for its dependencies, and when one of them
//! is published again, with a new rebuild counter, the package is no
//! longer up to date.
//!
//! A step first publishes every build that finished since the last one,
//! on every target, then writes the index of each repository that
//! changed, once, and only then queues builds. Writing an index costs
//! as much as the repository is large, so it is not done per build; and
//! a build is queued only when the index lists what it depends on.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::Instant;

use ci_http::server::{Request, Response};
use ci_json::Value;
use ci_protocol::Outcome;

use super::{json_ok, read_json, reject, str_field, Coordinator, Reject};
use crate::db::{ArchivedRelease, Intake, JobStatus, PackageBuild};
use crate::gitrefs;

/// Where a job runs.
#[derive(Debug, Clone)]
struct Place {
    pool: String,
    os: String,
    arch: String,
    /// The job's description, to which parameters are added.
    job: Value,
}

#[derive(Debug, Clone)]
struct Target {
    name: String,
    place: Place,
    /// The repository its packages are published into.
    repository: String,
    /// Where job containers reach that repository.
    repository_url: String,
    /// The platform rosdep keys are resolved for.
    platform_os: String,
    platform_version: String,
    skip_keys: Vec<String>,
    /// Whether builds for this target run the packages' tests. Without
    /// tests, a package does not wait for what only its tests need.
    tests: bool,
    /// The package names a build may publish, with `{name}` standing for
    /// the ROS package's name with dashes.
    names: Vec<String>,
}

#[derive(Debug, Clone)]
struct Config {
    enabled: bool,
    rosdistro: String,
    /// The rosdistro's repository and branch, to ask for its commit.
    rosdistro_repository: String,
    rosdistro_branch: String,
    /// Where the rosdistro's files are read from, without the commit.
    rosdistro_files: String,
    poll_seconds: i64,
    /// How many times a failed build is tried again.
    build_retries: i64,
    /// The delay before the first retry; it doubles each time.
    retry_backoff_seconds: i64,
    check: Place,
    archive: Place,
    targets: Vec<Target>,
}

fn text(value: &Value, name: &str) -> Result<String, String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
        .map(str::to_string)
        .ok_or_else(|| format!("\"{name}\" must be a line of text"))
}

fn number(value: &Value, name: &str, default: i64) -> Result<i64, String> {
    match value.get(name) {
        None => Ok(default),
        Some(given) => given
            .as_i64()
            .filter(|n| *n >= 0)
            .ok_or_else(|| format!("\"{name}\" must be a whole number")),
    }
}

fn strings(value: &Value, name: &str) -> Vec<String> {
    value
        .get(name)
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn list<'a>(value: &'a Value, name: &str) -> &'a [Value] {
    value
        .get(name)
        .and_then(Value::as_array)
        .unwrap_or_default()
}

fn word<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("")
}

fn place(value: &Value, what: &str) -> Result<Place, String> {
    let label = |name: &str| {
        text(value, name)
            .ok()
            .filter(|text| super::is_label(text))
            .ok_or_else(|| format!("{what}: \"{name}\" must be a short lower-case name"))
    };
    let job = value
        .get("job")
        .filter(|job| job.as_object().is_some())
        .ok_or_else(|| format!("{what}: \"job\" must be a job description"))?;
    Ok(Place {
        pool: label("pool")?,
        os: label("os")?,
        arch: label("arch")?,
        job: job.clone(),
    })
}

impl Config {
    fn parse(value: &Value) -> Result<Config, String> {
        let part = |name: &str| {
            value
                .get(name)
                .ok_or_else(|| format!("\"{name}\" is missing"))
        };
        let mut targets = Vec::new();
        for target in list(value, "targets") {
            let name = text(target, "name")?;
            let platform = target
                .get("platform")
                .ok_or_else(|| format!("target {name}: \"platform\" is missing"))?;
            let names = strings(target, "names");
            if names.is_empty() {
                return Err(format!("target {name}: \"names\" is missing"));
            }
            targets.push(Target {
                place: place(target, &format!("target {name}"))?,
                repository: text(target, "repository")?,
                repository_url: text(target, "repository_url")?,
                platform_os: text(platform, "os")?,
                platform_version: text(platform, "version")?,
                skip_keys: strings(target, "skip_keys"),
                tests: target.get("tests").and_then(Value::as_bool).unwrap_or(true),
                names,
                name,
            });
        }
        if targets.is_empty() {
            return Err("\"targets\" must name at least one target".to_string());
        }
        let mut seen = BTreeSet::new();
        if !targets.iter().all(|target| seen.insert(&target.name)) {
            return Err("two targets have the same name".to_string());
        }
        Ok(Config {
            enabled: value
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            rosdistro: text(value, "rosdistro")?,
            rosdistro_repository: text(value, "rosdistro_repository")?,
            rosdistro_branch: text(value, "rosdistro_branch")?,
            rosdistro_files: text(value, "rosdistro_files")?,
            poll_seconds: number(value, "poll_seconds", 300)?.max(10),
            build_retries: number(value, "build_retries", 2)?,
            retry_backoff_seconds: number(value, "retry_backoff_seconds", 600)?.max(1),
            check: place(part("check")?, "check")?,
            archive: place(part("archive")?, "archive")?,
            targets,
        })
    }

    fn rosdistro_url(&self, commit: &str) -> String {
        format!("{}/{commit}", self.rosdistro_files.trim_end_matches('/'))
    }
}

/// A job description with parameters added, and its inputs set if given.
fn job_with(job: &Value, parameters: Vec<(&str, Value)>, inputs: Option<Value>) -> Value {
    let mut members: Vec<(String, Value)> = job.as_object().unwrap_or_default().to_vec();
    let mut merged = match members.iter().position(|(name, _)| name == "parameters") {
        Some(index) => match members.remove(index).1 {
            Value::Object(existing) => existing,
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    for (name, value) in parameters {
        merged.retain(|(existing, _)| existing != name);
        merged.push((name.to_string(), value));
    }
    members.push(("parameters".to_string(), Value::Object(merged)));
    if let Some(inputs) = inputs {
        members.retain(|(name, _)| name != "inputs");
        members.push(("inputs".to_string(), inputs));
    }
    Value::Object(members)
}

/// A package the rosdistro says should exist.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wanted {
    repository: String,
    version: String,
    /// Where its release came from, as the archive recorded it.
    url: String,
    tag: String,
    /// The hash of the archived tarball of its release.
    sha256: String,
    /// The packages it depends on, of those that are wanted too.
    depends: Vec<String>,
    /// The packages only its tests depend on.
    test_depends: Vec<String>,
    /// The groups it is a member of.
    groups: Vec<String>,
}

/// Puts packages in an order in which each comes after everything it
/// depends on. Returns the order and the packages that could not be
/// placed, which are the ones in or behind a dependency cycle.
fn build_order(packages: &BTreeMap<String, Wanted>) -> (Vec<String>, Vec<String>) {
    let mut placed = BTreeSet::new();
    let mut order = Vec::new();
    loop {
        let ready: Vec<&String> = packages
            .iter()
            .filter(|(name, wanted)| {
                !placed.contains(*name) && wanted.depends.iter().all(|dep| placed.contains(dep))
            })
            .map(|(name, _)| name)
            .collect();
        if ready.is_empty() {
            break;
        }
        for name in ready {
            placed.insert(name.clone());
            order.push(name.clone());
        }
    }
    let stuck = packages
        .keys()
        .filter(|name| !placed.contains(*name))
        .cloned()
        .collect();
    (order, stuck)
}

/// What a build of a package is made from: the release's source, and the
/// published build of each dependency. Two builds with the same inputs
/// would be the same build.
fn inputs_of(wanted: &Wanted, published: &BTreeMap<String, (String, i64)>) -> Option<String> {
    let mut inputs = format!("{} {}", wanted.version, wanted.sha256);
    for dep in &wanted.depends {
        let (version, revision) = published.get(dep)?;
        inputs.push_str(&format!(" {dep}={version}-{revision}"));
    }
    Some(inputs)
}

/// The packages the rosdistro says should exist, by name.
type WantedPackages = BTreeMap<String, Wanted>;

/// What a step did, for one line in the log.
#[derive(Default)]
struct Done {
    published: usize,
    failed: usize,
    indexes: usize,
    queued: usize,
}

/// Where a target's packages stand in the middle of a step: after its
/// finished builds were published, before anything is queued.
struct Progress {
    /// The packages, each with what it waits for on this target.
    wanted: WantedPackages,
    order: Vec<String>,
    stuck: Vec<String>,
    rows: BTreeMap<String, PackageBuild>,
    /// The published build of each package: its version and counter.
    published: BTreeMap<String, (String, i64)>,
    /// The packages whose build is still queued or running.
    unfinished: BTreeSet<String>,
}

impl Coordinator {
    fn reconciler_config(&self) -> Result<Option<Config>, String> {
        let Some(stored) = self.db.reconciler_config().map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let value = ci_json::parse(&stored).map_err(|e| e.to_string())?;
        Config::parse(&value).map(Some)
    }

    /// One step of the reconciler. Called every few seconds.
    pub fn reconcile(&self) -> Result<(), String> {
        let Some(config) = self.reconciler_config()? else {
            return Ok(());
        };
        if !config.enabled {
            return Ok(());
        }
        let _guard = self.reconciling.lock().unwrap_or_else(|e| e.into_inner());
        self.intake_step(&config)?;
        let Some((commit, wanted)) = self.wanted_packages(&config)? else {
            return Ok(());
        };
        // The same for every job of this step, so it is made once, and
        // only if a job is queued.
        let distribution = OnceCell::new();
        let mut done = Done::default();
        // First every target's finished builds are published, then each
        // repository's index is written once, and only then is anything
        // queued: a build is queued when the index lists what it
        // depends on.
        let started = Instant::now();
        let mut progress = Vec::new();
        for target in &config.targets {
            progress.push(self.finished_builds(&config, target, &wanted, &mut done)?);
        }
        let published = Instant::now();
        let repositories: BTreeSet<&str> = config
            .targets
            .iter()
            .map(|target| target.repository.as_str())
            .collect();
        for repository in repositories {
            let written = self
                .write_stale_index(repository)
                .map_err(|Reject(_, message)| {
                    format!("writing the index of {repository}: {message}")
                })?;
            done.indexes += usize::from(written);
        }
        let indexed = Instant::now();
        for (target, progress) in config.targets.iter().zip(progress) {
            self.queue_builds(&config, target, &commit, progress, &distribution, &mut done)?;
        }
        if done.published + done.failed + done.queued > 0 {
            eprintln!(
                "reconciler: step: published {} builds in {} ms, {} failed, wrote {} indexes in {} ms, queued {} builds in {} ms",
                done.published,
                (published - started).as_millis(),
                done.failed,
                done.indexes,
                (indexed - published).as_millis(),
                done.queued,
                indexed.elapsed().as_millis(),
            );
        }
        Ok(())
    }

    fn submit_internal(&self, place: &Place, description: &Value) -> Result<i64, String> {
        self.submit_description(&place.pool, &place.os, &place.arch, description)
            .map_err(|Reject(_, message)| message)
    }

    /// A job's `results.json`, if it left one.
    fn job_results(&self, job: &JobStatus) -> Option<Value> {
        job.artifacts.iter().find(|a| a.path == "results.json")?;
        let path = self.artifact_dir(job.id, job.attempt).join("results.json");
        ci_json::parse_bytes(&fs::read(path).ok()?).ok()
    }

    // -----------------------------------------------------------------
    // Intake
    // -----------------------------------------------------------------

    fn intake_step(&self, config: &Config) -> Result<(), String> {
        let latest = self.db.latest_intake().map_err(|e| e.to_string())?;
        match &latest {
            Some(intake) if intake.state == "checking" => self.intake_checked(config, intake),
            Some(intake) if intake.state == "archiving" => self.intake_archived(config, intake),
            _ => {
                let asked = self.db.take_intake_request().map_err(|e| e.to_string())?;
                if !asked
                    && !self
                        .db
                        .reconciler_poll_due(config.poll_seconds)
                        .map_err(|e| e.to_string())?
                {
                    return Ok(());
                }
                let commit = gitrefs::branch_commit(
                    &self.forges,
                    &config.rosdistro_repository,
                    &config.rosdistro_branch,
                )?;
                if !asked && latest.is_some_and(|intake| intake.commit == commit) {
                    return Ok(());
                }
                self.intake_start(config, &commit)
            }
        }
    }

    fn intake_start(&self, config: &Config, commit: &str) -> Result<(), String> {
        // What the archive holds decides which releases are new. A
        // record without dependencies is from before they were recorded,
        // and is treated as missing so that it is completed.
        let archived: Vec<Value> = self
            .db
            .archived_releases(&config.rosdistro)
            .map_err(|e| e.to_string())?
            .iter()
            .filter(|release| release.record.contains("\"test_depends\""))
            .filter_map(|release| ci_json::parse(&release.record).ok())
            .collect();
        let mut platforms: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        for target in &config.targets {
            platforms
                .entry((target.platform_os.clone(), target.platform_version.clone()))
                .or_default()
                .extend(target.skip_keys.iter().cloned());
        }
        let targets = platforms
            .into_iter()
            .map(|((os, version), skip_keys)| {
                Value::object([
                    ("os", Value::from(os)),
                    ("version", Value::from(version)),
                    ("required", Value::Bool(true)),
                    ("skip_keys", Value::strings(skip_keys)),
                ])
            })
            .collect();
        let description = job_with(
            &config.check.job,
            vec![
                ("job", Value::from("release_check")),
                ("rosdistro", Value::from(config.rosdistro.as_str())),
                ("rosdistro_url", Value::from(config.rosdistro_url(commit))),
                ("archived", Value::Array(archived)),
                ("targets", Value::Array(targets)),
            ],
            None,
        );
        let job_id = self.submit_internal(&config.check, &description)?;
        self.db
            .start_intake(commit, job_id)
            .map_err(|e| e.to_string())?;
        eprintln!("reconciler: rosdistro commit {commit}: release check is job {job_id}");
        Ok(())
    }

    fn finish_intake(&self, intake: &Intake, state: &str, detail: Value) -> Result<(), String> {
        eprintln!(
            "reconciler: rosdistro commit {}: {state}{}",
            intake.commit,
            match word(&detail, "message") {
                "" => String::new(),
                message => format!(" ({message})"),
            }
        );
        self.db
            .update_intake(intake.id, state, &detail.to_json())
            .map_err(|e| e.to_string())
    }

    /// The release check has been queued; see whether it is done.
    fn intake_checked(&self, config: &Config, intake: &Intake) -> Result<(), String> {
        let Some(job) = self
            .db
            .job_status(intake.check_job)
            .map_err(|e| e.to_string())?
        else {
            return self.finish_intake(
                intake,
                "failed",
                Value::object([("message", Value::from("the release check job is gone"))]),
            );
        };
        if job.state != "finished" {
            return Ok(());
        }
        let results = self.job_results(&job).unwrap_or(Value::Null);
        let mut detail = vec![
            (
                "releases",
                Value::Array(list(&results, "releases").to_vec()),
            ),
            (
                "problems",
                Value::Array(list(&results, "problems").to_vec()),
            ),
            (
                "moved_tags",
                Value::Array(list(&results, "moved_tags").to_vec()),
            ),
        ];
        let succeeded = job.outcome.as_deref() == Some(Outcome::Success.as_str())
            && word(&results, "status") == "success";
        if !succeeded {
            let message = match word(&results, "message") {
                "" => format!("the release check, job {}, did not succeed", job.id),
                message => format!("the release check found {message}"),
            };
            detail.push(("message", Value::from(message)));
            return self.finish_intake(intake, "failed", Value::object(detail));
        }
        let commit = intake.commit.as_str();
        let mut archive_jobs = Vec::new();
        for release in list(&results, "releases") {
            if word(release, "state") != "new" {
                continue;
            }
            let named = ["repository", "url", "version", "tag", "commit"];
            let mut parameters = vec![
                ("job", Value::from("source_archive")),
                ("rosdistro", Value::from(config.rosdistro.as_str())),
                ("rosdistro_url", Value::from(config.rosdistro_url(commit))),
                ("skip", Value::Array(list(release, "skipped").to_vec())),
            ];
            let mut entry = Vec::new();
            for name in named {
                parameters.push((name, Value::from(word(release, name))));
                entry.push((name, Value::from(word(release, name))));
            }
            let job_id = self.submit_internal(
                &config.archive,
                &job_with(&config.archive.job, parameters, None),
            )?;
            entry.push(("job_id", Value::Int(job_id)));
            archive_jobs.push(Value::object(entry));
        }
        let state = if archive_jobs.is_empty() {
            "done"
        } else {
            "archiving"
        };
        detail.push(("archive_jobs", Value::Array(archive_jobs)));
        self.finish_intake(intake, state, Value::object(detail))
    }

    /// The source archive jobs have been queued; see whether they are
    /// done, and take their tarballs in.
    fn intake_archived(&self, config: &Config, intake: &Intake) -> Result<(), String> {
        let detail = ci_json::parse(&intake.detail).map_err(|e| e.to_string())?;
        let mut jobs = Vec::new();
        for entry in list(&detail, "archive_jobs") {
            let id = entry.get("job_id").and_then(Value::as_i64).unwrap_or(0);
            match self.db.job_status(id).map_err(|e| e.to_string())? {
                Some(job) if job.state == "finished" => jobs.push((entry, job)),
                Some(_) => return Ok(()),
                None => return Err(format!("source archive job {id} is gone")),
            }
        }
        let mut failure = None;
        for (entry, job) in jobs {
            let release = format!("{} {}", word(entry, "repository"), word(entry, "version"));
            if job.outcome.as_deref() != Some(Outcome::Success.as_str()) {
                failure = Some(format!(
                    "{release}: the source archive job, {}, did not succeed",
                    job.id
                ));
                continue;
            }
            let mut body = vec![
                ("job_id", Value::Int(job.id)),
                ("rosdistro", Value::from(config.rosdistro.as_str())),
            ];
            for name in ["repository", "url", "version", "tag", "commit"] {
                body.push((name, Value::from(word(entry, name))));
            }
            if let Err(Reject(_, message)) = self.archive_job("reconciler", &Value::object(body)) {
                failure = Some(format!("{release}: {message}"));
            }
        }
        let mut members: Vec<(String, Value)> = detail.as_object().unwrap_or_default().to_vec();
        match failure {
            Some(message) => {
                members.push(("message".to_string(), Value::from(message)));
                self.finish_intake(intake, "failed", Value::Object(members))
            }
            None => self.finish_intake(intake, "done", Value::Object(members)),
        }
    }

    /// The packages of the last commit that was taken in whole, with the
    /// commit. `None` until there is one.
    fn wanted_packages(&self, config: &Config) -> Result<Option<(String, WantedPackages)>, String> {
        let Some(intake) = self.db.last_done_intake().map_err(|e| e.to_string())? else {
            return Ok(None);
        };
        let detail = ci_json::parse(&intake.detail).map_err(|e| e.to_string())?;
        let mut wanted = BTreeMap::new();
        let mut keys: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
        let releases: Vec<(&str, &str)> = list(&detail, "releases")
            .iter()
            .map(|release| (word(release, "repository"), word(release, "version")))
            .collect();
        let archive: BTreeMap<(String, String), ArchivedRelease> = self
            .db
            .archived_releases_of(&config.rosdistro, &releases)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|release| {
                (
                    (release.repository.clone(), release.version.clone()),
                    release,
                )
            })
            .collect();
        for (repository, version) in releases {
            let Some(archived) = archive.get(&(repository.to_string(), version.to_string())) else {
                return Err(format!(
                    "{repository} {version} is not in the source archive"
                ));
            };
            let record = ci_json::parse(&archived.record).map_err(|e| e.to_string())?;
            for package in list(&record, "packages") {
                let name = word(package, "name").to_string();
                keys.insert(
                    name.clone(),
                    (
                        strings(package, "depends"),
                        strings(package, "test_depends"),
                    ),
                );
                wanted.insert(
                    name,
                    Wanted {
                        repository: repository.to_string(),
                        version: version.to_string(),
                        url: word(&record, "url").to_string(),
                        tag: word(&record, "tag").to_string(),
                        sha256: archived.sha256.clone(),
                        depends: Vec::new(),
                        test_depends: Vec::new(),
                        groups: strings(package, "groups"),
                    },
                );
            }
        }
        // A key that is not one of these packages is something the
        // platform provides, and orders nothing.
        let names: BTreeSet<String> = wanted.keys().cloned().collect();
        for (name, package) in &mut wanted {
            let among = |keys: &[String]| -> Vec<String> {
                keys.iter()
                    .filter(|key| names.contains(*key) && *key != name)
                    .cloned()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            };
            package.depends = among(&keys[name].0);
            package.test_depends = among(&keys[name].1);
        }
        Ok(Some((intake.commit, wanted)))
    }

    // -----------------------------------------------------------------
    // Builds
    // -----------------------------------------------------------------

    /// The first half of a target's step: sees how far each queued build
    /// has come and publishes those that are done, without writing the
    /// index.
    fn finished_builds(
        &self,
        config: &Config,
        target: &Target,
        wanted: &BTreeMap<String, Wanted>,
        done: &mut Done,
    ) -> Result<Progress, String> {
        let err = |e: ci_postgres::Error| e.to_string();
        // What a package waits for depends on whether its tests run.
        let wanted: WantedPackages = wanted
            .iter()
            .map(|(name, package)| {
                let mut package = package.clone();
                if target.tests {
                    let test_depends = std::mem::take(&mut package.test_depends);
                    package.depends.extend(test_depends);
                    package.depends.sort();
                    package.depends.dedup();
                }
                (name.clone(), package)
            })
            .collect();
        let mut published: BTreeMap<String, (String, i64)> = self
            .db
            .repository_builds(&target.repository)
            .map_err(err)?
            .into_iter()
            .filter(|build| build.arch == target.place.arch)
            .map(|build| (build.package, (build.version, build.revision)))
            .collect();
        let mut rows: BTreeMap<String, PackageBuild> = self
            .db
            .package_builds(&target.name)
            .map_err(err)?
            .into_iter()
            .map(|row| (row.package.clone(), row))
            .collect();
        // A package that is no longer wanted is forgotten here. What was
        // published of it stays in the repository.
        for name in rows.keys().filter(|name| !wanted.contains_key(*name)) {
            self.db
                .forget_package_build(&target.name, name)
                .map_err(err)?;
        }
        let (order, stuck) = build_order(&wanted);

        // The builds that were queued: see how far each has come,
        // and publish those that are done.
        let building: Vec<i64> = rows
            .values()
            .filter(|row| row.state == "building")
            .filter_map(|row| row.job_id)
            .collect();
        let states = self.db.job_states(&building).map_err(err)?;
        let mut unfinished = BTreeSet::new();
        let mut later = Vec::new();
        for name in &order {
            let Some(before) = rows.get(name).filter(|row| row.state == "building") else {
                continue;
            };
            let mut row = before.clone();
            let mut retry_in = None;
            let state = row.job_id.and_then(|id| states.get(&id));
            if let Some(state) = state.filter(|state| *state != "finished") {
                row.status = if state == "queued" {
                    "queued"
                } else {
                    "building"
                }
                .to_string();
                unfinished.insert(name.clone());
            } else {
                let job = match row.job_id.filter(|_| state.is_some()) {
                    Some(id) => self.db.job_status(id).map_err(err)?,
                    None => None,
                };
                match job {
                    Some(job) if job.outcome.as_deref() == Some(Outcome::Success.as_str()) => {
                        let body = Value::object([
                            ("job_id", Value::Int(job.id)),
                            ("package", Value::from(name.as_str())),
                            ("version", Value::from(row.version.as_str())),
                            ("revision", Value::Int(row.revision)),
                            (
                                "names",
                                Value::strings(
                                    target
                                        .names
                                        .iter()
                                        .map(|n| n.replace("{name}", &name.replace('_', "-"))),
                                ),
                            ),
                        ]);
                        let result =
                            self.publish_without_index("reconciler", &target.repository, &body);
                        match result {
                            Ok(_) => {
                                row.state = "published".to_string();
                                row.message.clear();
                                published.insert(name.clone(), (row.version.clone(), row.revision));
                                done.published += 1;
                            }
                            // Something of the coordinator's own went
                            // wrong. The build is fine; try again later.
                            Err(Reject(status, message)) if status >= 500 => {
                                return Err(format!(
                                    "publishing {name} for {}: {message}",
                                    target.name
                                ));
                            }
                            Err(Reject(_, message)) => {
                                row.state = "failed".to_string();
                                row.attempts += 1;
                                row.message =
                                    format!("job {} could not be published: {message}", job.id);
                            }
                        }
                    }
                    finished => {
                        row.state = "failed".to_string();
                        row.attempts += 1;
                        row.message = match &finished {
                            Some(job) => {
                                let results = self.job_results(job).unwrap_or(Value::Null);
                                let outcome = job.outcome.as_deref().unwrap_or("unknown");
                                match (word(&results, "status"), word(&results, "message")) {
                                    ("", _) => format!("job {}: {outcome}", job.id),
                                    (status, "") => format!("job {}: {status}", job.id),
                                    (status, message) => {
                                        format!("job {}: {status}: {message}", job.id)
                                    }
                                }
                            }
                            None => "the build job is gone".to_string(),
                        };
                    }
                }
                if row.state == "failed" {
                    // The delay doubles with each attempt.
                    let doublings = (row.attempts - 1).clamp(0, 16) as u32;
                    retry_in = Some(config.retry_backoff_seconds << doublings);
                    row.retry_due = false;
                    done.failed += 1;
                    eprintln!(
                        "reconciler: {}: {name} failed, attempt {}: {}",
                        target.name, row.attempts, row.message
                    );
                }
            }
            self.save_build(target, Some(before), &row, retry_in, &mut later)?;
            rows.insert(name.clone(), row);
        }

        self.db
            .save_package_statuses(&target.name, &later)
            .map_err(err)?;
        Ok(Progress {
            wanted,
            order,
            stuck,
            rows,
            published,
            unfinished,
        })
    }

    /// The second half of a target's step, after the index is written:
    /// decides what each package needs and queues the builds that can
    /// start.
    fn queue_builds(
        &self,
        config: &Config,
        target: &Target,
        commit: &str,
        progress: Progress,
        distribution: &OnceCell<Value>,
        done: &mut Done,
    ) -> Result<(), String> {
        let err = |e: ci_postgres::Error| e.to_string();
        let Progress {
            wanted,
            order,
            stuck,
            rows,
            published,
            unfinished,
        } = progress;
        let wanted = &wanted;
        let mut up_to_date = BTreeSet::new();
        let mut later = Vec::new();
        for name in &order {
            if unfinished.contains(name) {
                continue;
            }
            let package = &wanted[name];
            let before = rows.get(name);
            let mut row = before.cloned().unwrap_or_else(|| PackageBuild {
                package: name.clone(),
                state: "none".to_string(),
                status: String::new(),
                inputs: String::new(),
                version: String::new(),
                revision: 0,
                job_id: None,
                attempts: 0,
                retry_due: true,
                next_retry_at: None,
                message: String::new(),
            });
            let retry_in = None;

            // Then decide what the package needs.
            let dependencies_ready = package.depends.iter().all(|dep| up_to_date.contains(dep));
            let inputs = if dependencies_ready {
                inputs_of(package, &published)
            } else {
                None
            };
            let Some(inputs) = inputs else {
                row.status = "blocked".to_string();
                self.save_build(target, before, &row, retry_in, &mut later)?;
                continue;
            };
            let same = row.inputs == inputs;
            if row.state == "published" && same {
                row.status = "up_to_date".to_string();
                up_to_date.insert(name.clone());
                self.save_build(target, before, &row, retry_in, &mut later)?;
                continue;
            }
            if row.state == "failed" && same {
                // Retried a few times, to cover a build that fails by
                // chance. After that it stays failed until its source or
                // a dependency changes, which changes the inputs.
                if row.attempts > config.build_retries || !row.retry_due {
                    row.status = "failed".to_string();
                    self.save_build(target, before, &row, retry_in, &mut later)?;
                    continue;
                }
            } else {
                row.attempts = 0;
            }

            // Queue a build.
            let revision = self
                .db
                .allocate_revision(&target.repository, name, &package.version)
                .map_err(err)?;
            let description = self.build_description(
                config,
                target,
                commit,
                wanted,
                distribution,
                name,
                revision,
            )?;
            let job_id = self.submit_internal(&target.place, &description)?;
            row.state = "building".to_string();
            row.status = "queued".to_string();
            row.inputs = inputs;
            row.version = package.version.clone();
            row.revision = revision;
            row.job_id = Some(job_id);
            row.message.clear();
            done.queued += 1;
            eprintln!(
                "reconciler: {}: queued {name} {} revision {revision} as job {job_id}",
                target.name, package.version
            );
            self.save_build(target, before, &row, None, &mut later)?;
        }

        for name in &stuck {
            let before = rows.get(name);
            let mut row = before.cloned().unwrap_or_else(|| PackageBuild {
                package: name.clone(),
                state: "none".to_string(),
                status: String::new(),
                inputs: String::new(),
                version: String::new(),
                revision: 0,
                job_id: None,
                attempts: 0,
                retry_due: true,
                next_retry_at: None,
                message: String::new(),
            });
            row.status = "blocked".to_string();
            row.message = "it is part of a dependency cycle, or depends on one".to_string();
            self.save_build(target, before, &row, None, &mut later)?;
        }
        self.db
            .save_package_statuses(&target.name, &later)
            .map_err(err)?;
        Ok(())
    }

    /// Writes a package's row if anything in it changed. A change in
    /// nothing but how the package is shown, such as from blocked to up
    /// to date, is put on `later` instead, to be written with the others
    /// of its kind in one transaction: a large rebuild makes thousands
    /// of them in a step, and they follow from the rest, so one that is
    /// lost with a crash is made again by the next step.
    fn save_build(
        &self,
        target: &Target,
        before: Option<&PackageBuild>,
        row: &PackageBuild,
        retry_in: Option<i64>,
        later: &mut Vec<PackageBuild>,
    ) -> Result<(), String> {
        if before == Some(row) && retry_in.is_none() {
            return Ok(());
        }
        let shown_only = retry_in.is_none()
            && match before {
                Some(before) => {
                    let mut same = before.clone();
                    same.status.clone_from(&row.status);
                    same.message.clone_from(&row.message);
                    same == *row
                }
                None => row.state == "none",
            };
        if shown_only {
            later.push(row.clone());
            return Ok(());
        }
        self.db
            .save_package_build(&target.name, row, retry_in)
            .map_err(|e| e.to_string())
    }

    /// The releases of the distribution, with their packages: what
    /// rosdep is given as the distribution file.
    fn distribution(wanted: &WantedPackages) -> Value {
        let mut releases: BTreeMap<&str, (&Wanted, Vec<&str>)> = BTreeMap::new();
        let mut groups: BTreeMap<&str, Vec<(String, Value)>> = BTreeMap::new();
        for (name, package) in wanted {
            releases
                .entry(&package.repository)
                .or_insert((package, Vec::new()))
                .1
                .push(name);
            if !package.groups.is_empty() {
                groups
                    .entry(&package.repository)
                    .or_default()
                    .push((name.clone(), Value::strings(&package.groups)));
            }
        }
        Value::Array(
            releases
                .into_iter()
                .map(|(repository, (release, packages))| {
                    Value::object([
                        ("repository", Value::from(repository)),
                        ("url", Value::from(release.url.as_str())),
                        ("version", Value::from(release.version.as_str())),
                        ("tag", Value::from(release.tag.as_str())),
                        ("packages", Value::strings(packages)),
                        (
                            "groups",
                            Value::Object(groups.remove(repository).unwrap_or_default()),
                        ),
                    ])
                })
                .collect(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_description(
        &self,
        config: &Config,
        target: &Target,
        commit: &str,
        wanted: &BTreeMap<String, Wanted>,
        distribution: &OnceCell<Value>,
        package: &str,
        revision: i64,
    ) -> Result<Value, String> {
        let distribution = distribution.get_or_init(|| Self::distribution(wanted));
        let key = self
            .signing_key
            .as_ref()
            .ok_or("the coordinator has no signing key")?
            .public_key_armored();
        let mut parameters = vec![
            ("package", Value::from(package)),
            ("rosdistro", Value::from(config.rosdistro.as_str())),
            ("rosdistro_url", Value::from(config.rosdistro_url(commit))),
            ("revision", Value::Int(revision)),
            ("distribution", distribution.clone()),
            (
                "repositories",
                Value::Array(vec![Value::object([
                    ("url", Value::from(target.repository_url.as_str())),
                    ("key", Value::from(key)),
                ])]),
            ),
        ];
        if !target.skip_keys.is_empty() {
            parameters.push(("skip_keys", Value::strings(&target.skip_keys)));
        }
        if !target.tests {
            parameters.push(("test", Value::Bool(false)));
        }
        let inputs = Value::Array(vec![Value::object([
            ("name", Value::from("source.tar.gz")),
            (
                "url",
                Value::from(format!("archive:{}", wanted[package].sha256)),
            ),
        ])]);
        Ok(job_with(&target.place.job, parameters, Some(inputs)))
    }

    // -----------------------------------------------------------------
    // Admin API
    // -----------------------------------------------------------------

    pub(super) fn configure_reconciler(
        &self,
        admin: &str,
        request: &mut Request<'_>,
    ) -> Result<Response, Reject> {
        let body = read_json(request)?;
        let config = Config::parse(&body).map_err(|message| Reject(400, message))?;
        for target in &config.targets {
            if self.db.repository(&target.repository)?.is_none() {
                return Err(Reject(
                    400,
                    format!(
                        "target {}: there is no repository named {}",
                        target.name, target.repository
                    ),
                ));
            }
        }
        self.db.set_reconciler_config(&body.to_json())?;
        eprintln!(
            "admin {admin:?}: configured the reconciler: {} from {} branch {}, {} targets{}",
            config.rosdistro,
            config.rosdistro_repository,
            config.rosdistro_branch,
            config.targets.len(),
            if config.enabled { "" } else { ", switched off" }
        );
        Ok(json_ok(Value::object([("configured", Value::Bool(true))])))
    }

    /// Where everything stands: the last intake and every package of
    /// every target.
    pub(super) fn reconciler_status(&self) -> Result<Response, Reject> {
        let config = self
            .reconciler_config()
            .map_err(|message| Reject(500, message))?
            .ok_or_else(|| reject(404, "the reconciler is not configured"))?;
        let intake = match self.db.latest_intake()? {
            Some(intake) => {
                let detail = ci_json::parse(&intake.detail).unwrap_or(Value::Null);
                Value::object([
                    ("commit", Value::from(intake.commit)),
                    ("state", Value::from(intake.state)),
                    ("check_job", Value::Int(intake.check_job)),
                    ("started_at", Value::from(intake.created_at)),
                    ("message", Value::from(word(&detail, "message"))),
                    ("problems", Value::Array(list(&detail, "problems").to_vec())),
                    (
                        "moved_tags",
                        Value::Array(list(&detail, "moved_tags").to_vec()),
                    ),
                ])
            }
            None => Value::Null,
        };
        let built_from = self
            .db
            .last_done_intake()?
            .map_or(Value::Null, |intake| Value::from(intake.commit));
        let mut targets = Vec::new();
        for target in &config.targets {
            let rows = self.db.package_builds(&target.name)?;
            let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
            for row in &rows {
                *counts.entry(row.status.as_str()).or_default() += 1;
            }
            targets.push(Value::object([
                ("name", Value::from(target.name.as_str())),
                ("repository", Value::from(target.repository.as_str())),
                (
                    "counts",
                    Value::object(
                        counts
                            .into_iter()
                            .map(|(status, n)| (status, Value::Int(n))),
                    ),
                ),
                (
                    "packages",
                    Value::Array(
                        rows.iter()
                            .map(|row| {
                                Value::object([
                                    ("package", Value::from(row.package.as_str())),
                                    ("state", Value::from(row.status.as_str())),
                                    ("version", Value::from(row.version.as_str())),
                                    ("revision", Value::Int(row.revision)),
                                    ("job_id", row.job_id.map_or(Value::Null, Value::Int)),
                                    ("attempts", Value::Int(row.attempts)),
                                    (
                                        "next_retry_at",
                                        row.next_retry_at
                                            .as_deref()
                                            .map_or(Value::Null, Value::from),
                                    ),
                                    ("message", Value::from(row.message.as_str())),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]));
        }
        Ok(json_ok(Value::object([
            ("enabled", Value::Bool(config.enabled)),
            ("rosdistro", Value::from(config.rosdistro)),
            ("intake", intake),
            ("built_from_commit", built_from),
            (
                "last_step_milliseconds",
                Value::Int(
                    self.reconcile_millis
                        .load(std::sync::atomic::Ordering::Relaxed) as i64,
                ),
            ),
            ("targets", Value::Array(targets)),
        ])))
    }

    /// Makes the published build of a package count as out of date, so
    /// that it is built again, and with it everything that depends on it.
    pub(super) fn rebuild_package(
        &self,
        admin: &str,
        request: &mut Request<'_>,
    ) -> Result<Response, Reject> {
        let body = read_json(request)?;
        let package = str_field(&body, "package")?;
        let target = body.get("target").and_then(Value::as_str);
        let changed = self.db.mark_package_stale(package, target)?;
        if changed == 0 {
            return Err(reject(404, "no published build of that package"));
        }
        eprintln!("admin {admin:?}: asked for a rebuild of {package} on {changed} target(s)");
        Ok(json_ok(Value::object([(
            "targets",
            Value::Int(changed as i64),
        )])))
    }

    /// Makes the reconciler look at the rosdistro again now, and take
    /// its commit in again even if it is the one it last looked at.
    pub(super) fn retry_intake(&self, admin: &str) -> Result<Response, Reject> {
        self.db.ask_for_intake()?;
        eprintln!("admin {admin:?}: asked the reconciler to take the rosdistro in again");
        Ok(json_ok(Value::object([("asked", Value::Bool(true))])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted(version: &str, depends: &[&str]) -> Wanted {
        Wanted {
            repository: "repo".to_string(),
            version: version.to_string(),
            url: String::new(),
            tag: String::new(),
            sha256: "0".repeat(64),
            depends: depends.iter().map(|d| d.to_string()).collect(),
            test_depends: Vec::new(),
            groups: Vec::new(),
        }
    }

    fn packages(list: &[(&str, &[&str])]) -> BTreeMap<String, Wanted> {
        list.iter()
            .map(|(name, depends)| (name.to_string(), wanted("1.0.0", depends)))
            .collect()
    }

    #[test]
    fn packages_come_after_what_they_depend_on() {
        let (order, stuck) = build_order(&packages(&[
            ("c", &["b", "a"]),
            ("a", &[]),
            ("b", &["a"]),
            ("d", &[]),
        ]));
        assert_eq!(order, ["a", "d", "b", "c"]);
        assert!(stuck.is_empty());
    }

    #[test]
    fn a_cycle_holds_back_only_what_is_behind_it() {
        let (order, stuck) = build_order(&packages(&[
            ("a", &[]),
            ("x", &["y"]),
            ("y", &["x"]),
            ("z", &["x", "a"]),
        ]));
        assert_eq!(order, ["a"]);
        assert_eq!(stuck, ["x", "y", "z"]);
    }

    #[test]
    fn inputs_change_when_a_dependency_is_published_again() {
        let package = wanted("1.0.0", &["a", "b"]);
        let mut published = BTreeMap::new();
        assert_eq!(inputs_of(&package, &published), None);
        published.insert("a".to_string(), ("2.0.0".to_string(), 1));
        published.insert("b".to_string(), ("3.0.0".to_string(), 4));
        let first = inputs_of(&package, &published).unwrap();
        assert_eq!(
            first,
            format!("1.0.0 {} a=2.0.0-1 b=3.0.0-4", "0".repeat(64))
        );
        published.insert("a".to_string(), ("2.0.0".to_string(), 2));
        assert_ne!(inputs_of(&package, &published).unwrap(), first);
        // A new version of the package itself changes them too.
        published.insert("a".to_string(), ("2.0.0".to_string(), 1));
        assert_ne!(
            inputs_of(&wanted("1.0.1", &["a", "b"]), &published).unwrap(),
            first
        );
    }

    #[test]
    fn parameters_and_inputs_are_added_to_a_job() {
        let job =
            ci_json::parse(r#"{"image": "x", "parameters": {"test": false, "package": "old"}}"#)
                .unwrap();
        let made = job_with(
            &job,
            vec![("package", Value::from("new"))],
            Some(Value::Array(Vec::new())),
        );
        let parameters = made.get("parameters").unwrap();
        assert_eq!(word(parameters, "package"), "new");
        assert_eq!(parameters.get("test").and_then(Value::as_bool), Some(false));
        assert!(made.get("inputs").is_some());
        assert_eq!(word(&made, "image"), "x");
    }

    #[test]
    fn a_configuration_is_checked() {
        let good = r#"{
            "rosdistro": "rolling",
            "rosdistro_repository": "https://example.invalid/rosdistro.git",
            "rosdistro_branch": "main",
            "rosdistro_files": "https://raw.example.invalid/rosdistro",
            "check": {"pool": "pull-request", "os": "linux", "arch": "amd64", "job": {}},
            "archive": {"pool": "release", "os": "linux", "arch": "amd64", "job": {}},
            "targets": [{"name": "ubuntu", "pool": "release", "os": "linux", "arch": "amd64",
                         "job": {}, "repository": "r", "repository_url": "http://x/r",
                         "platform": {"os": "ubuntu", "version": "resolute"},
                         "names": ["ros-rolling-{name}"]}]
        }"#;
        let config = Config::parse(&ci_json::parse(good).unwrap()).unwrap();
        assert!(config.enabled);
        assert_eq!(config.build_retries, 2);
        assert_eq!(
            config.rosdistro_url("abc"),
            "https://raw.example.invalid/rosdistro/abc"
        );
        for (from, to) in [
            (r#""rosdistro": "rolling","#, ""),
            (r#""names": ["ros-rolling-{name}"]"#, r#""names": []"#),
            (r#""pool": "pull-request""#, r#""pool": "Pull Request""#),
        ] {
            let bad = good.replace(from, to);
            assert_ne!(bad, good);
            assert!(
                Config::parse(&ci_json::parse(&bad).unwrap()).is_err(),
                "{from}"
            );
        }
    }
}
