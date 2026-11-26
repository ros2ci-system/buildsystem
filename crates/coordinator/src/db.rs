// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The coordinator's database: the connection, the schema, and every
//! query.
//!
//! All state lives here. The coordinator process keeps nothing in memory
//! that it could not read back, so a restart loses nothing.

use std::sync::Mutex;

use ci_postgres::{Config, Connection, Error, Param, Row};
use ci_protocol::Outcome;

use crate::repo::{Format, Planned, Repository, Stored};

/// The advisory lock that marks the one active coordinator. The number is
/// arbitrary; it only has to be the same in every coordinator.
const COORDINATOR_LOCK: i64 = 0x726f_7332_6369;

/// Schema changes, in order. Each runs once, in its own transaction, and
/// they only ever move forward: a released migration is never edited.
const MIGRATIONS: &[&str] = &[
    // 1: workers, jobs and artifacts.
    "CREATE TABLE worker_tokens (
        id bigserial PRIMARY KEY,
        name text NOT NULL,
        pool text NOT NULL,
        token_hash text NOT NULL UNIQUE,
        revoked boolean NOT NULL DEFAULT false,
        created_at timestamptz NOT NULL DEFAULT now()
    );
    CREATE TABLE workers (
        id bigserial PRIMARY KEY,
        token_id bigint NOT NULL REFERENCES worker_tokens (id),
        session_hash text NOT NULL UNIQUE,
        pool text NOT NULL,
        os text NOT NULL,
        arch text NOT NULL,
        registered_at timestamptz NOT NULL DEFAULT now(),
        last_seen_at timestamptz NOT NULL DEFAULT now()
    );
    CREATE TABLE jobs (
        id bigserial PRIMARY KEY,
        pool text NOT NULL,
        os text NOT NULL,
        arch text NOT NULL,
        description text NOT NULL,
        state text NOT NULL DEFAULT 'queued'
            CHECK (state IN ('queued', 'running', 'finished')),
        outcome text,
        exit_code bigint,
        message text NOT NULL DEFAULT '',
        image_id text NOT NULL DEFAULT '',
        attempt bigint NOT NULL DEFAULT 0,
        infrastructure_failures bigint NOT NULL DEFAULT 0,
        worker_id bigint REFERENCES workers (id),
        lease_expires_at timestamptz,
        not_before timestamptz NOT NULL DEFAULT now(),
        cancel_requested boolean NOT NULL DEFAULT false,
        log_bytes bigint NOT NULL DEFAULT 0,
        created_at timestamptz NOT NULL DEFAULT now(),
        started_at timestamptz,
        finished_at timestamptz
    );
    CREATE INDEX jobs_queue ON jobs (pool, os, arch, id) WHERE state = 'queued';
    CREATE INDEX jobs_running ON jobs (lease_expires_at) WHERE state = 'running';
    CREATE TABLE artifacts (
        job_id bigint NOT NULL REFERENCES jobs (id),
        attempt bigint NOT NULL,
        path text NOT NULL,
        size bigint NOT NULL,
        sha256 text NOT NULL,
        PRIMARY KEY (job_id, attempt, path)
    );",
    // 2: tokens for the admin API.
    "CREATE TABLE admin_tokens (
        id bigserial PRIMARY KEY,
        name text NOT NULL,
        token_hash text NOT NULL UNIQUE,
        revoked boolean NOT NULL DEFAULT false,
        created_at timestamptz NOT NULL DEFAULT now()
    );",
    // 3: files stored on the coordinator for jobs to read.
    "CREATE TABLE stored_files (
        name text PRIMARY KEY,
        size bigint NOT NULL,
        sha256 text NOT NULL,
        updated_at timestamptz NOT NULL DEFAULT now()
    );",
    // 4: workers that have been asked to stop once they are idle.
    "ALTER TABLE workers ADD COLUMN draining boolean NOT NULL DEFAULT false;",
    // 5: package repositories. A build is one package for one
    // architecture; publishing it again replaces its files.
    "CREATE TABLE repositories (
        name text PRIMARY KEY,
        format text NOT NULL CHECK (format IN ('apt', 'rpm')),
        suite text NOT NULL,
        version_suffix text NOT NULL,
        architectures text NOT NULL,
        source_arch text NOT NULL,
        created_at timestamptz NOT NULL DEFAULT now()
    );
    CREATE TABLE repository_revisions (
        repository text NOT NULL REFERENCES repositories (name),
        package text NOT NULL,
        version text NOT NULL,
        revision bigint NOT NULL,
        PRIMARY KEY (repository, package, version)
    );
    CREATE TABLE repository_builds (
        repository text NOT NULL REFERENCES repositories (name),
        package text NOT NULL,
        arch text NOT NULL,
        version text NOT NULL,
        revision bigint NOT NULL,
        job_id bigint NOT NULL REFERENCES jobs (id),
        attempt bigint NOT NULL,
        published_at timestamptz NOT NULL DEFAULT now(),
        PRIMARY KEY (repository, package, arch)
    );
    CREATE TABLE repository_files (
        repository text NOT NULL REFERENCES repositories (name),
        package text NOT NULL,
        arch text NOT NULL,
        path text NOT NULL,
        section text NOT NULL,
        size bigint NOT NULL,
        sha256 text NOT NULL,
        metadata text NOT NULL,
        PRIMARY KEY (repository, path)
    );",
    // 6: the source archive. One entry per version of a repository in a
    // ROS distribution, never replaced.
    "CREATE TABLE source_archive (
        rosdistro text NOT NULL,
        repository text NOT NULL,
        version text NOT NULL,
        commit_id text NOT NULL,
        sha256 text NOT NULL,
        record text NOT NULL,
        job_id bigint NOT NULL REFERENCES jobs (id),
        attempt bigint NOT NULL,
        archived_at timestamptz NOT NULL DEFAULT now(),
        PRIMARY KEY (rosdistro, repository, version)
    );
    CREATE INDEX source_archive_sha256 ON source_archive (sha256);",
    // 7: the reconciler. Its settings, each rosdistro commit it took in,
    // and the last build of each package for each target.
    "CREATE TABLE reconciler (
        id bigint PRIMARY KEY CHECK (id = 1),
        config text NOT NULL,
        polled_at timestamptz,
        intake_asked boolean NOT NULL DEFAULT false
    );
    CREATE TABLE intakes (
        id bigserial PRIMARY KEY,
        commit_id text NOT NULL,
        state text NOT NULL CHECK (state IN ('checking', 'archiving', 'done', 'failed')),
        check_job bigint NOT NULL REFERENCES jobs (id),
        detail text NOT NULL DEFAULT '{}',
        created_at timestamptz NOT NULL DEFAULT now(),
        finished_at timestamptz
    );
    CREATE TABLE package_builds (
        target text NOT NULL,
        package text NOT NULL,
        state text NOT NULL,
        status text NOT NULL,
        inputs text NOT NULL,
        version text NOT NULL,
        revision bigint NOT NULL,
        job_id bigint REFERENCES jobs (id),
        attempts bigint NOT NULL DEFAULT 0,
        next_retry_at timestamptz,
        message text NOT NULL DEFAULT '',
        updated_at timestamptz NOT NULL DEFAULT now(),
        PRIMARY KEY (target, package)
    );",
];

const TIMESTAMP: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

pub struct Db {
    config: Config,
    connection: Mutex<Option<Connection>>,
}

#[derive(Debug, Clone)]
pub struct Worker {
    pub id: i64,
    pub pool: String,
    pub os: String,
    pub arch: String,
    /// An administrator asked this worker to stop once it is idle.
    pub draining: bool,
}

/// A worker as the admin API lists it.
#[derive(Debug, Clone)]
pub struct WorkerStatus {
    pub id: i64,
    pub token_id: i64,
    pub token_name: String,
    pub pool: String,
    pub os: String,
    pub arch: String,
    pub registered_at: String,
    pub last_seen_at: String,
    pub draining: bool,
    /// The job it is running, if any.
    pub job_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub id: i64,
    pub attempt: i64,
    pub description: String,
}

/// The parts of a job row that decide what a worker's call may do.
#[derive(Debug, Clone)]
pub struct JobLock {
    pub state: String,
    pub attempt: i64,
    pub worker_id: Option<i64>,
    pub cancel_requested: bool,
    pub log_bytes: i64,
    pub infrastructure_failures: i64,
}

impl JobLock {
    /// Whether the job is running as this attempt on this worker.
    pub fn held_by(&self, worker_id: i64, attempt: i64) -> bool {
        self.state == "running" && self.attempt == attempt && self.worker_id == Some(worker_id)
    }
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub path: String,
    pub size: i64,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct JobStatus {
    pub id: i64,
    pub pool: String,
    pub os: String,
    pub arch: String,
    pub state: String,
    pub outcome: Option<String>,
    pub exit_code: Option<i64>,
    pub message: String,
    pub image_id: String,
    pub attempt: i64,
    pub infrastructure_failures: i64,
    pub cancel_requested: bool,
    pub log_bytes: i64,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub artifacts: Vec<Artifact>,
}

/// A package build that a repository holds.
#[derive(Debug, Clone)]
pub struct PublishedBuild {
    pub package: String,
    pub arch: String,
    pub version: String,
    pub revision: i64,
    pub job_id: i64,
    pub attempt: i64,
    pub published_at: String,
}

/// A rosdistro commit the reconciler took in, or is taking in.
#[derive(Debug, Clone)]
pub struct Intake {
    pub id: i64,
    pub commit: String,
    /// `checking`, `archiving`, `done` or `failed`.
    pub state: String,
    pub check_job: i64,
    /// What the release check found and which archive jobs run, as JSON.
    pub detail: String,
    pub created_at: String,
}

/// The last build of a package for a target, as the reconciler keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageBuild {
    pub package: String,
    /// Where the last build stands: `none`, `building`, `published`,
    /// `failed`, or `stale` when a rebuild was asked for.
    pub state: String,
    /// The package's state as the design names them, for showing.
    pub status: String,
    /// What the last build was made from.
    pub inputs: String,
    pub version: String,
    pub revision: i64,
    pub job_id: Option<i64>,
    /// How many times a build from these inputs has failed.
    pub attempts: i64,
    /// Whether the time for the next retry has come.
    pub retry_due: bool,
    pub next_retry_at: Option<String>,
    pub message: String,
}

/// A release in the source archive.
#[derive(Debug, Clone)]
pub struct ArchivedRelease {
    pub rosdistro: String,
    pub repository: String,
    pub version: String,
    pub commit: String,
    /// The tarball's hash, which is also its name in the archive.
    pub sha256: String,
    /// Everything recorded about the release, as JSON.
    pub record: String,
}

/// What happened to a job that failed for an infrastructure reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requeue {
    /// It went back in the queue, to start after a delay.
    Requeued,
    /// It had used up its retries and is finished.
    GaveUp,
}

impl Db {
    /// Connects, takes the coordinator lock and brings the schema up to
    /// date.
    pub fn open(config: Config) -> Result<Db, String> {
        let db = Db {
            config,
            connection: Mutex::new(None),
        };
        db.with(migrate).map_err(|e| e.to_string())?;
        Ok(db)
    }

    /// Runs `body` with the connection, reconnecting first if the last
    /// use left it broken.
    pub fn with<T>(
        &self,
        body: impl FnOnce(&mut Connection) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut guard = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.as_ref().is_some_and(Connection::is_usable) {
            *guard = None;
            let mut connection = Connection::connect(&self.config)?;
            // The lock belongs to the session, so a new session has to
            // take it again. If another coordinator took it meanwhile,
            // this one must not carry on beside it.
            let locked = connection
                .query(
                    "SELECT pg_try_advisory_lock($1)",
                    &[COORDINATOR_LOCK.into()],
                )?
                .first()
                .map(|row| row.bool(0))
                .transpose()?
                .unwrap_or(false);
            if !locked {
                return Err(Error::Config(
                    "another coordinator is already running against this database".to_string(),
                ));
            }
            *guard = Some(connection);
        }
        let connection = guard.as_mut().expect("the connection was just set");
        body(connection)
    }

    /// Records a stored file, replacing any earlier one of that name.
    pub fn put_stored_file(&self, file: &Artifact) -> Result<(), Error> {
        self.with(|c| {
            c.execute(
                "INSERT INTO stored_files (name, size, sha256) VALUES ($1, $2, $3)
                 ON CONFLICT (name) DO UPDATE
                 SET size = excluded.size, sha256 = excluded.sha256, updated_at = now()",
                &[(&file.path).into(), file.size.into(), (&file.sha256).into()],
            )
            .map(|_| ())
        })
    }

    /// A stored file's size and hash, with its name in `path`.
    pub fn stored_file(&self, name: &str) -> Result<Option<Artifact>, Error> {
        self.with(|c| {
            c.query(
                "SELECT name, size, sha256 FROM stored_files WHERE name = $1",
                &[name.into()],
            )?
            .first()
            .map(artifact_from_row)
            .transpose()
        })
    }

    pub fn stored_files(&self) -> Result<Vec<Artifact>, Error> {
        self.with(|c| {
            c.query(
                "SELECT name, size, sha256 FROM stored_files ORDER BY name",
                &[],
            )?
            .iter()
            .map(artifact_from_row)
            .collect()
        })
    }

    /// Forgets a stored file. Returns `false` when there was none.
    pub fn delete_stored_file(&self, name: &str) -> Result<bool, Error> {
        self.with(|c| {
            Ok(c.execute("DELETE FROM stored_files WHERE name = $1", &[name.into()])? > 0)
        })
    }

    /// Records a new repository. Returns `false` when the name is taken.
    pub fn create_repository(&self, repository: &Repository) -> Result<bool, Error> {
        self.with(|c| {
            Ok(c.execute(
                "INSERT INTO repositories (name, format, suite, version_suffix, architectures, source_arch)
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (name) DO NOTHING",
                &[
                    (&repository.name).into(),
                    repository.format.as_str().into(),
                    (&repository.suite).into(),
                    (&repository.version_suffix).into(),
                    (&repository.architectures.join(" ")).into(),
                    (&repository.source_arch).into(),
                ],
            )? > 0)
        })
    }

    pub fn repository(&self, name: &str) -> Result<Option<Repository>, Error> {
        self.with(|c| {
            c.query(
                "SELECT name, format, suite, version_suffix, architectures, source_arch
                 FROM repositories WHERE name = $1",
                &[name.into()],
            )?
            .first()
            .map(|row| {
                Ok(Repository {
                    name: row.text(0)?.to_string(),
                    format: Format::parse(row.text(1)?).ok_or_else(|| {
                        Error::Protocol("a repository has an unknown format".to_string())
                    })?,
                    suite: row.text(2)?.to_string(),
                    version_suffix: row.text(3)?.to_string(),
                    architectures: row.text(4)?.split(' ').map(str::to_string).collect(),
                    source_arch: row.text(5)?.to_string(),
                })
            })
            .transpose()
        })
    }

    /// Hands out the next rebuild counter for a version of a package:
    /// 1 the first time, and one more each time after. A counter is
    /// never given twice, whether or not its build is ever published.
    pub fn allocate_revision(
        &self,
        repository: &str,
        package: &str,
        version: &str,
    ) -> Result<i64, Error> {
        self.with(|c| {
            c.query(
                "INSERT INTO repository_revisions (repository, package, version, revision)
                 VALUES ($1, $2, $3, 1)
                 ON CONFLICT (repository, package, version)
                 DO UPDATE SET revision = repository_revisions.revision + 1
                 RETURNING revision",
                &[repository.into(), package.into(), version.into()],
            )?
            .first()
            .ok_or_else(no_row)?
            .int(0)
        })
    }

    /// The highest rebuild counter handed out so far, if any.
    pub fn allocated_revision(
        &self,
        repository: &str,
        package: &str,
        version: &str,
    ) -> Result<Option<i64>, Error> {
        self.with(|c| {
            c.query(
                "SELECT revision FROM repository_revisions
                 WHERE repository = $1 AND package = $2 AND version = $3",
                &[repository.into(), package.into(), version.into()],
            )?
            .first()
            .map(|row| row.int(0))
            .transpose()
        })
    }

    pub fn repository_builds(&self, repository: &str) -> Result<Vec<PublishedBuild>, Error> {
        self.with(|c| {
            c.query(
                &format!(
                    "SELECT package, arch, version, revision, job_id, attempt,
                            to_char(published_at AT TIME ZONE 'UTC', {TIMESTAMP})
                     FROM repository_builds WHERE repository = $1 ORDER BY package, arch"
                ),
                &[repository.into()],
            )?
            .iter()
            .map(|row| {
                Ok(PublishedBuild {
                    package: row.text(0)?.to_string(),
                    arch: row.text(1)?.to_string(),
                    version: row.text(2)?.to_string(),
                    revision: row.int(3)?,
                    job_id: row.int(4)?,
                    attempt: row.int(5)?,
                    published_at: row.text(6)?.to_string(),
                })
            })
            .collect()
        })
    }

    /// Makes a build the published one for its package and architecture,
    /// in one transaction. Returns the paths of the files it replaced.
    #[allow(clippy::too_many_arguments)]
    pub fn replace_repository_build(
        &self,
        repository: &str,
        package: &str,
        arch: &str,
        version: &str,
        revision: i64,
        job_id: i64,
        attempt: i64,
        files: &[Planned],
    ) -> Result<Vec<String>, Error> {
        self.with(|c| {
            c.transaction(|c| {
                let replaced = c
                    .query(
                        "DELETE FROM repository_files
                         WHERE repository = $1 AND package = $2 AND arch = $3 RETURNING path",
                        &[repository.into(), package.into(), arch.into()],
                    )?
                    .iter()
                    .map(|row| row.text(0).map(str::to_string))
                    .collect::<Result<Vec<_>, _>>()?;
                for file in files {
                    c.execute(
                        "INSERT INTO repository_files
                             (repository, package, arch, path, section, size, sha256, metadata)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                        &[
                            repository.into(),
                            package.into(),
                            arch.into(),
                            (&file.path).into(),
                            (&file.section).into(),
                            file.size.into(),
                            (&file.sha256).into(),
                            (&file.metadata).into(),
                        ],
                    )?;
                }
                c.execute(
                    "INSERT INTO repository_builds
                         (repository, package, arch, version, revision, job_id, attempt)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (repository, package, arch) DO UPDATE
                     SET version = excluded.version, revision = excluded.revision,
                         job_id = excluded.job_id, attempt = excluded.attempt,
                         published_at = now()",
                    &[
                        repository.into(),
                        package.into(),
                        arch.into(),
                        version.into(),
                        revision.into(),
                        job_id.into(),
                        attempt.into(),
                    ],
                )?;
                Ok(replaced)
            })
        })
    }

    pub fn archive_release(
        &self,
        release: &ArchivedRelease,
        job_id: i64,
        attempt: i64,
    ) -> Result<(), Error> {
        self.with(|c| {
            c.execute(
                "INSERT INTO source_archive
                     (rosdistro, repository, version, commit_id, sha256, record, job_id, attempt)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    (&release.rosdistro).into(),
                    (&release.repository).into(),
                    (&release.version).into(),
                    (&release.commit).into(),
                    (&release.sha256).into(),
                    (&release.record).into(),
                    job_id.into(),
                    attempt.into(),
                ],
            )
            .map(|_| ())
        })
    }

    /// Replaces what is recorded about an archived release. The tarball
    /// and the commit it came from are not touched.
    pub fn complete_archive_record(
        &self,
        rosdistro: &str,
        repository: &str,
        version: &str,
        record: &str,
    ) -> Result<(), Error> {
        self.with(|c| {
            c.execute(
                "UPDATE source_archive SET record = $4
                 WHERE rosdistro = $1 AND repository = $2 AND version = $3",
                &[
                    rosdistro.into(),
                    repository.into(),
                    version.into(),
                    record.into(),
                ],
            )
            .map(|_| ())
        })
    }

    pub fn archived_release(
        &self,
        rosdistro: &str,
        repository: &str,
        version: &str,
    ) -> Result<Option<ArchivedRelease>, Error> {
        self.with(|c| {
            c.query(
                "SELECT rosdistro, repository, version, commit_id, sha256, record
                 FROM source_archive
                 WHERE rosdistro = $1 AND repository = $2 AND version = $3",
                &[rosdistro.into(), repository.into(), version.into()],
            )?
            .first()
            .map(archived_release_from_row)
            .transpose()
        })
    }

    /// Every archived release of a ROS distribution, oldest first.
    pub fn archived_releases(&self, rosdistro: &str) -> Result<Vec<ArchivedRelease>, Error> {
        self.with(|c| {
            c.query(
                "SELECT rosdistro, repository, version, commit_id, sha256, record
                 FROM source_archive WHERE rosdistro = $1
                 ORDER BY archived_at, repository, version",
                &[rosdistro.into()],
            )?
            .iter()
            .map(archived_release_from_row)
            .collect()
        })
    }

    /// Whether any release was archived as a tarball of this hash.
    pub fn archived_tarball(&self, sha256: &str) -> Result<bool, Error> {
        self.with(|c| {
            Ok(!c
                .query(
                    "SELECT 1 FROM source_archive WHERE sha256 = $1 LIMIT 1",
                    &[sha256.into()],
                )?
                .is_empty())
        })
    }

    pub fn reconciler_config(&self) -> Result<Option<String>, Error> {
        self.with(|c| {
            c.query("SELECT config FROM reconciler WHERE id = 1", &[])?
                .first()
                .map(|row| row.text(0).map(str::to_string))
                .transpose()
        })
    }

    /// Stores the reconciler's settings, and makes it look at the
    /// rosdistro at once.
    pub fn set_reconciler_config(&self, config: &str) -> Result<(), Error> {
        self.with(|c| {
            c.execute(
                "INSERT INTO reconciler (id, config) VALUES (1, $1)
                 ON CONFLICT (id) DO UPDATE SET config = excluded.config, polled_at = NULL",
                &[config.into()],
            )
            .map(|_| ())
        })
    }

    /// Whether it is time to ask the rosdistro for its commit again. If
    /// it is, the time of asking is recorded.
    pub fn reconciler_poll_due(&self, seconds: i64) -> Result<bool, Error> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE reconciler SET polled_at = now()
                 WHERE id = 1 AND (polled_at IS NULL
                     OR polled_at < now() - $1::bigint * interval '1 second')",
                &[seconds.into()],
            )? > 0)
        })
    }

    /// Records that someone asked for the rosdistro to be taken in again.
    pub fn ask_for_intake(&self) -> Result<(), Error> {
        self.with(|c| {
            c.execute(
                "UPDATE reconciler SET intake_asked = true WHERE id = 1",
                &[],
            )
            .map(|_| ())
        })
    }

    /// Whether someone asked for the rosdistro to be taken in again.
    /// Asking is used up by this.
    pub fn take_intake_request(&self) -> Result<bool, Error> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE reconciler SET intake_asked = false WHERE id = 1 AND intake_asked",
                &[],
            )? > 0)
        })
    }

    fn intake_where(&self, condition: &str) -> Result<Option<Intake>, Error> {
        self.with(|c| {
            c.query(
                &format!(
                    "SELECT id, commit_id, state, check_job, detail,
                            to_char(created_at AT TIME ZONE 'UTC', {TIMESTAMP})
                     FROM intakes {condition} ORDER BY id DESC LIMIT 1"
                ),
                &[],
            )?
            .first()
            .map(|row| {
                Ok(Intake {
                    id: row.int(0)?,
                    commit: row.text(1)?.to_string(),
                    state: row.text(2)?.to_string(),
                    check_job: row.int(3)?,
                    detail: row.text(4)?.to_string(),
                    created_at: row.text(5)?.to_string(),
                })
            })
            .transpose()
        })
    }

    /// The rosdistro commit the reconciler looked at last.
    pub fn latest_intake(&self) -> Result<Option<Intake>, Error> {
        self.intake_where("")
    }

    /// The last rosdistro commit that was taken in whole.
    pub fn last_done_intake(&self) -> Result<Option<Intake>, Error> {
        self.intake_where("WHERE state = 'done'")
    }

    pub fn start_intake(&self, commit: &str, check_job: i64) -> Result<(), Error> {
        self.with(|c| {
            c.execute(
                "INSERT INTO intakes (commit_id, state, check_job) VALUES ($1, 'checking', $2)",
                &[commit.into(), check_job.into()],
            )
            .map(|_| ())
        })
    }

    pub fn update_intake(&self, id: i64, state: &str, detail: &str) -> Result<(), Error> {
        self.with(|c| {
            c.execute(
                "UPDATE intakes SET state = $2, detail = $3,
                        finished_at = CASE WHEN $2 IN ('done', 'failed') THEN now() END
                 WHERE id = $1",
                &[id.into(), state.into(), detail.into()],
            )
            .map(|_| ())
        })
    }

    /// The reconciler's record of every package of a target.
    pub fn package_builds(&self, target: &str) -> Result<Vec<PackageBuild>, Error> {
        self.with(|c| {
            c.query(
                &format!(
                    "SELECT package, state, status, inputs, version, revision, job_id, attempts,
                            coalesce(next_retry_at <= now(), true),
                            to_char(next_retry_at AT TIME ZONE 'UTC', {TIMESTAMP}), message
                     FROM package_builds WHERE target = $1 ORDER BY package"
                ),
                &[target.into()],
            )?
            .iter()
            .map(|row| {
                Ok(PackageBuild {
                    package: row.text(0)?.to_string(),
                    state: row.text(1)?.to_string(),
                    status: row.text(2)?.to_string(),
                    inputs: row.text(3)?.to_string(),
                    version: row.text(4)?.to_string(),
                    revision: row.int(5)?,
                    job_id: row.opt_int(6)?,
                    attempts: row.int(7)?,
                    retry_due: row.bool(8)?,
                    next_retry_at: row.get(9).map(str::to_string),
                    message: row.text(10)?.to_string(),
                })
            })
            .collect()
        })
    }

    /// Writes a package's record. With `retry_in`, the next retry is
    /// set to that many seconds from now; otherwise it is left alone.
    pub fn save_package_build(
        &self,
        target: &str,
        build: &PackageBuild,
        retry_in: Option<i64>,
    ) -> Result<(), Error> {
        self.with(|c| {
            c.execute(
                "INSERT INTO package_builds
                     (target, package, state, status, inputs, version, revision, job_id,
                      attempts, message, next_retry_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                         now() + $11::bigint * interval '1 second')
                 ON CONFLICT (target, package) DO UPDATE
                 SET state = excluded.state, status = excluded.status, inputs = excluded.inputs,
                     version = excluded.version, revision = excluded.revision,
                     job_id = excluded.job_id, attempts = excluded.attempts,
                     message = excluded.message, updated_at = now(),
                     next_retry_at = coalesce(excluded.next_retry_at, package_builds.next_retry_at)",
                &[
                    target.into(),
                    (&build.package).into(),
                    (&build.state).into(),
                    (&build.status).into(),
                    (&build.inputs).into(),
                    (&build.version).into(),
                    build.revision.into(),
                    build.job_id.into(),
                    build.attempts.into(),
                    (&build.message).into(),
                    retry_in.into(),
                ],
            )
            .map(|_| ())
        })
    }

    pub fn forget_package_build(&self, target: &str, package: &str) -> Result<(), Error> {
        self.with(|c| {
            c.execute(
                "DELETE FROM package_builds WHERE target = $1 AND package = $2",
                &[target.into(), package.into()],
            )
            .map(|_| ())
        })
    }

    /// Makes the last build of a package count as out of date, on one
    /// target or on all. Returns how many targets that changed.
    pub fn mark_package_stale(&self, package: &str, target: Option<&str>) -> Result<u64, Error> {
        self.with(|c| {
            c.execute(
                "UPDATE package_builds SET state = 'stale', updated_at = now()
                 WHERE package = $1 AND state IN ('published', 'failed')
                   AND ($2::text IS NULL OR target = $2)",
                &[package.into(), target.into()],
            )
        })
    }

    /// Every file a repository holds, as its index needs them.
    pub fn repository_files(&self, repository: &str) -> Result<Vec<Stored>, Error> {
        self.with(|c| {
            c.query(
                "SELECT path, section, metadata FROM repository_files WHERE repository = $1",
                &[repository.into()],
            )?
            .iter()
            .map(|row| {
                Ok(Stored {
                    path: row.text(0)?.to_string(),
                    section: row.text(1)?.to_string(),
                    metadata: row.text(2)?.to_string(),
                })
            })
            .collect()
        })
    }

    /// The description of a job, if it is running as this attempt on
    /// this worker.
    pub fn held_job_description(
        &self,
        job_id: i64,
        worker_id: i64,
        attempt: i64,
    ) -> Result<Option<String>, Error> {
        self.with(|c| {
            c.query(
                "SELECT description FROM jobs
                 WHERE id = $1 AND state = 'running' AND attempt = $2 AND worker_id = $3",
                &[job_id.into(), attempt.into(), worker_id.into()],
            )?
            .first()
            .map(|row| row.text(0).map(str::to_string))
            .transpose()
        })
    }

    pub fn create_admin_token(&self, name: &str, token_hash: &str) -> Result<i64, Error> {
        self.with(|c| {
            c.query(
                "INSERT INTO admin_tokens (name, token_hash) VALUES ($1, $2) RETURNING id",
                &[name.into(), token_hash.into()],
            )?
            .first()
            .ok_or_else(no_row)?
            .int(0)
        })
    }

    /// Creates the first admin token, if there is no usable one. Returns
    /// whether it did. The check and the insert are one statement, so two
    /// callers cannot both create a first token.
    pub fn create_first_admin_token(&self, name: &str, token_hash: &str) -> Result<bool, Error> {
        self.with(|c| {
            let created = c.execute(
                "INSERT INTO admin_tokens (name, token_hash)
                 SELECT $1, $2 WHERE NOT EXISTS (SELECT 1 FROM admin_tokens WHERE NOT revoked)",
                &[name.into(), token_hash.into()],
            )?;
            Ok(created > 0)
        })
    }

    /// The id and name of the admin token with this hash, unless it has
    /// been revoked.
    pub fn authenticate_admin(&self, token_hash: &str) -> Result<Option<(i64, String)>, Error> {
        self.with(|c| {
            let rows = c.query(
                "SELECT id, name FROM admin_tokens WHERE token_hash = $1 AND NOT revoked",
                &[token_hash.into()],
            )?;
            rows.first()
                .map(|row| Ok((row.int(0)?, row.text(1)?.to_string())))
                .transpose()
        })
    }

    /// Revokes an admin token. Returns `false` when there is no such
    /// token or it was already revoked.
    pub fn revoke_admin_token(&self, id: i64) -> Result<bool, Error> {
        self.with(|c| {
            let changed = c.execute(
                "UPDATE admin_tokens SET revoked = true WHERE id = $1 AND NOT revoked",
                &[id.into()],
            )?;
            Ok(changed > 0)
        })
    }

    /// Revokes a worker enrolment token. Workers that registered with it
    /// are refused from then on. Returns `false` when there is no such
    /// token or it was already revoked.
    pub fn revoke_worker_token(&self, id: i64) -> Result<bool, Error> {
        self.with(|c| {
            let changed = c.execute(
                "UPDATE worker_tokens SET revoked = true WHERE id = $1 AND NOT revoked",
                &[id.into()],
            )?;
            Ok(changed > 0)
        })
    }

    pub fn create_worker_token(
        &self,
        name: &str,
        pool: &str,
        token_hash: &str,
    ) -> Result<i64, Error> {
        self.with(|c| {
            c.query(
                "INSERT INTO worker_tokens (name, pool, token_hash) VALUES ($1, $2, $3) RETURNING id",
                &[name.into(), pool.into(), token_hash.into()],
            )?
            .first()
            .ok_or_else(no_row)?
            .int(0)
        })
    }

    /// Exchanges an enrolment token for a worker record. Returns `None`
    /// when the token is unknown or revoked.
    pub fn register_worker(
        &self,
        token_hash: &str,
        session_hash: &str,
        os: &str,
        arch: &str,
    ) -> Result<Option<Worker>, Error> {
        self.with(|c| {
            let rows = c.query(
                "INSERT INTO workers (token_id, session_hash, pool, os, arch)
                 SELECT id, $2, pool, $3, $4 FROM worker_tokens
                 WHERE token_hash = $1 AND NOT revoked
                 RETURNING id, pool, os, arch, draining",
                &[
                    token_hash.into(),
                    session_hash.into(),
                    os.into(),
                    arch.into(),
                ],
            )?;
            rows.first().map(worker_from_row).transpose()
        })
    }

    /// Finds the worker a session token belongs to, and notes that it
    /// was seen. A worker whose enrolment token has been revoked is no
    /// longer known.
    pub fn authenticate_worker(&self, session_hash: &str) -> Result<Option<Worker>, Error> {
        self.with(|c| {
            let rows = c.query(
                "UPDATE workers SET last_seen_at = now()
                 WHERE session_hash = $1
                   AND NOT (SELECT revoked FROM worker_tokens WHERE id = workers.token_id)
                 RETURNING id, pool, os, arch, draining",
                &[session_hash.into()],
            )?;
            rows.first().map(worker_from_row).transpose()
        })
    }

    /// Marks a worker as draining: it is given no more jobs, and is told
    /// to shut down the next time it asks for one. Returns `false` when
    /// there is no such worker. Draining a worker twice is not an error.
    pub fn drain_worker(&self, id: i64) -> Result<bool, Error> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE workers SET draining = true WHERE id = $1",
                &[id.into()],
            )? > 0)
        })
    }

    /// Every worker whose enrolment token still stands, newest first.
    pub fn workers(&self) -> Result<Vec<WorkerStatus>, Error> {
        self.with(|c| {
            c.query(
                &format!(
                    "SELECT w.id, t.id, t.name, w.pool, w.os, w.arch,
                            to_char(w.registered_at AT TIME ZONE 'UTC', {TIMESTAMP}),
                            to_char(w.last_seen_at AT TIME ZONE 'UTC', {TIMESTAMP}),
                            w.draining,
                            (SELECT max(j.id) FROM jobs j
                             WHERE j.state = 'running' AND j.worker_id = w.id)
                     FROM workers w JOIN worker_tokens t ON t.id = w.token_id
                     WHERE NOT t.revoked
                     ORDER BY w.id DESC"
                ),
                &[],
            )?
            .iter()
            .map(|row| {
                Ok(WorkerStatus {
                    id: row.int(0)?,
                    token_id: row.int(1)?,
                    token_name: row.text(2)?.to_string(),
                    pool: row.text(3)?.to_string(),
                    os: row.text(4)?.to_string(),
                    arch: row.text(5)?.to_string(),
                    registered_at: row.text(6)?.to_string(),
                    last_seen_at: row.text(7)?.to_string(),
                    draining: row.bool(8)?,
                    job_id: row.opt_int(9)?,
                })
            })
            .collect()
        })
    }

    pub fn submit_job(
        &self,
        pool: &str,
        os: &str,
        arch: &str,
        description: &str,
    ) -> Result<i64, Error> {
        self.with(|c| {
            c.query(
                "INSERT INTO jobs (pool, os, arch, description) VALUES ($1, $2, $3, $4) RETURNING id",
                &[pool.into(), os.into(), arch.into(), description.into()],
            )?
            .first()
            .ok_or_else(no_row)?
            .int(0)
        })
    }

    /// Hands the oldest waiting job that fits the worker to it, as a new
    /// attempt with a fresh lease.
    pub fn claim_job(
        &self,
        worker: &Worker,
        lease_seconds: i64,
    ) -> Result<Option<ClaimedJob>, Error> {
        self.with(|c| {
            let rows = c.query(
                "UPDATE jobs SET state = 'running', attempt = attempt + 1, worker_id = $1,
                        lease_expires_at = now() + $2::bigint * interval '1 second',
                        started_at = now(), log_bytes = 0
                 WHERE id = (
                     SELECT id FROM jobs
                     WHERE state = 'queued' AND pool = $3 AND os = $4 AND arch = $5
                       AND not_before <= now()
                     ORDER BY id LIMIT 1
                     FOR UPDATE SKIP LOCKED
                 )
                 RETURNING id, attempt, description",
                &[
                    worker.id.into(),
                    lease_seconds.into(),
                    (&worker.pool).into(),
                    (&worker.os).into(),
                    (&worker.arch).into(),
                ],
            )?;
            rows.first()
                .map(|row| {
                    Ok(ClaimedJob {
                        id: row.int(0)?,
                        attempt: row.int(1)?,
                        description: row.text(2)?.to_string(),
                    })
                })
                .transpose()
        })
    }

    /// Whether the job is running as this attempt on this worker, without
    /// locking it. For calls that only need to check before doing slow
    /// work, such as an artifact upload.
    pub fn job_held_by(&self, job_id: i64, worker_id: i64, attempt: i64) -> Result<bool, Error> {
        self.with(|c| {
            let rows = c.query(
                "SELECT 1 FROM jobs WHERE id = $1 AND state = 'running' AND attempt = $2 AND worker_id = $3",
                &[job_id.into(), attempt.into(), worker_id.into()],
            )?;
            Ok(!rows.is_empty())
        })
    }

    pub fn record_artifact(
        &self,
        job_id: i64,
        attempt: i64,
        artifact: &Artifact,
    ) -> Result<(), Error> {
        self.with(|c| {
            c.execute(
                "INSERT INTO artifacts (job_id, attempt, path, size, sha256) VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (job_id, attempt, path) DO UPDATE SET size = excluded.size, sha256 = excluded.sha256",
                &[
                    job_id.into(),
                    attempt.into(),
                    (&artifact.path).into(),
                    artifact.size.into(),
                    (&artifact.sha256).into(),
                ],
            )
            .map(|_| ())
        })
    }

    /// Marks a job for cancellation. A waiting job is finished at once; a
    /// running one is told at its next Report. Returns `false` when there
    /// is no such job.
    pub fn cancel_job(&self, job_id: i64) -> Result<bool, Error> {
        self.with(|c| {
            let changed = c.execute(
                "UPDATE jobs SET
                    cancel_requested = true,
                    state = CASE WHEN state = 'queued' THEN 'finished' ELSE state END,
                    outcome = CASE WHEN state = 'queued' THEN 'cancelled' ELSE outcome END,
                    finished_at = CASE WHEN state = 'queued' THEN now() ELSE finished_at END
                 WHERE id = $1",
                &[job_id.into()],
            )?;
            Ok(changed > 0)
        })
    }

    pub fn job_status(&self, job_id: i64) -> Result<Option<JobStatus>, Error> {
        self.with(|c| {
            let rows = c.query(
                &format!(
                    "SELECT id, pool, os, arch, state, outcome, exit_code, message, image_id, attempt,
                            infrastructure_failures, cancel_requested, log_bytes,
                            to_char(created_at AT TIME ZONE 'UTC', {TIMESTAMP}),
                            to_char(started_at AT TIME ZONE 'UTC', {TIMESTAMP}),
                            to_char(finished_at AT TIME ZONE 'UTC', {TIMESTAMP})
                     FROM jobs WHERE id = $1"
                ),
                &[job_id.into()],
            )?;
            let Some(row) = rows.first() else {
                return Ok(None);
            };
            let attempt = row.int(9)?;
            let artifacts = c
                .query(
                    "SELECT path, size, sha256 FROM artifacts WHERE job_id = $1 AND attempt = $2 ORDER BY path",
                    &[job_id.into(), attempt.into()],
                )?
                .iter()
                .map(artifact_from_row)
                .collect::<Result<_, _>>()?;
            Ok(Some(JobStatus {
                id: row.int(0)?,
                pool: row.text(1)?.to_string(),
                os: row.text(2)?.to_string(),
                arch: row.text(3)?.to_string(),
                state: row.text(4)?.to_string(),
                outcome: row.get(5).map(str::to_string),
                exit_code: row.opt_int(6)?,
                message: row.text(7)?.to_string(),
                image_id: row.text(8)?.to_string(),
                attempt,
                infrastructure_failures: row.int(10)?,
                cancel_requested: row.bool(11)?,
                log_bytes: row.int(12)?,
                created_at: row.text(13)?.to_string(),
                started_at: row.get(14).map(str::to_string),
                finished_at: row.get(15).map(str::to_string),
                artifacts,
            }))
        })
    }

    /// Gives every running job a fresh lease. Called at start-up, so that
    /// time the coordinator spent down is not counted against workers
    /// that could not reach it.
    pub fn extend_all_leases(&self, lease_seconds: i64) -> Result<u64, Error> {
        self.with(|c| {
            c.execute(
                "UPDATE jobs SET lease_expires_at = now() + $1::bigint * interval '1 second'
                 WHERE state = 'running'",
                &[lease_seconds.into()],
            )
        })
    }

    /// Finds running jobs whose worker has stopped reporting and counts
    /// an infrastructure failure against each.
    pub fn expire_leases(
        &self,
        max_failures: i64,
        backoff_seconds: i64,
    ) -> Result<Vec<(i64, Requeue)>, Error> {
        self.with(|c| {
            c.transaction(|c| {
                let expired = c.query(
                    "SELECT id, infrastructure_failures FROM jobs
                     WHERE state = 'running' AND lease_expires_at < now()
                     FOR UPDATE SKIP LOCKED",
                    &[],
                )?;
                let mut results = Vec::new();
                for row in expired.iter() {
                    let id = row.int(0)?;
                    let failures = row.int(1)?;
                    let result = infrastructure_failure(
                        c,
                        id,
                        failures,
                        "the worker stopped reporting",
                        "",
                        max_failures,
                        backoff_seconds,
                    )?;
                    results.push((id, result));
                }
                Ok(results)
            })
        })
    }
}

/// Reads the parts of a job row that decide what a call may do, and
/// locks the row until the transaction ends.
pub fn lock_job(c: &mut Connection, job_id: i64) -> Result<Option<JobLock>, Error> {
    let rows = c.query(
        "SELECT state, attempt, worker_id, cancel_requested, log_bytes, infrastructure_failures
         FROM jobs WHERE id = $1 FOR UPDATE",
        &[job_id.into()],
    )?;
    rows.first()
        .map(|row| {
            Ok(JobLock {
                state: row.text(0)?.to_string(),
                attempt: row.int(1)?,
                worker_id: row.opt_int(2)?,
                cancel_requested: row.bool(3)?,
                log_bytes: row.int(4)?,
                infrastructure_failures: row.int(5)?,
            })
        })
        .transpose()
}

/// Renews a job's lease and records how much log has been stored.
pub fn renew_lease(
    c: &mut Connection,
    job_id: i64,
    log_bytes: i64,
    lease_seconds: i64,
) -> Result<(), Error> {
    c.execute(
        "UPDATE jobs SET lease_expires_at = now() + $2::bigint * interval '1 second', log_bytes = $3
         WHERE id = $1",
        &[job_id.into(), lease_seconds.into(), log_bytes.into()],
    )
    .map(|_| ())
}

pub fn finish_job(
    c: &mut Connection,
    job_id: i64,
    outcome: Outcome,
    exit_code: Option<i64>,
    message: &str,
    image_id: &str,
) -> Result<(), Error> {
    c.execute(
        "UPDATE jobs SET state = 'finished', outcome = $2, exit_code = $3, message = $4, image_id = $5,
                finished_at = now(), lease_expires_at = NULL
         WHERE id = $1",
        &[
            job_id.into(),
            outcome.as_str().into(),
            exit_code.into(),
            message.into(),
            image_id.into(),
        ],
    )
    .map(|_| ())
}

/// Counts an infrastructure failure against a job the caller has locked.
/// The job goes back in the queue with a growing delay, until it has
/// failed this way `max_failures` times.
pub fn infrastructure_failure(
    c: &mut Connection,
    job_id: i64,
    failures_so_far: i64,
    message: &str,
    image_id: &str,
    max_failures: i64,
    backoff_seconds: i64,
) -> Result<Requeue, Error> {
    let failures = failures_so_far + 1;
    if failures >= max_failures {
        c.execute(
            "UPDATE jobs SET infrastructure_failures = $2 WHERE id = $1",
            &[job_id.into(), failures.into()],
        )?;
        finish_job(
            c,
            job_id,
            Outcome::InfrastructureFailure,
            None,
            message,
            image_id,
        )?;
        return Ok(Requeue::GaveUp);
    }
    // 1x, 2x, 4x the base delay, and so on.
    let delay = backoff_seconds.saturating_mul(1i64 << (failures - 1).min(20));
    c.execute(
        "UPDATE jobs SET state = 'queued', worker_id = NULL, lease_expires_at = NULL,
                infrastructure_failures = $2, message = $3,
                not_before = now() + $4::bigint * interval '1 second'
         WHERE id = $1",
        &[job_id.into(), failures.into(), message.into(), delay.into()],
    )?;
    Ok(Requeue::Requeued)
}

pub fn artifacts_of(c: &mut Connection, job_id: i64, attempt: i64) -> Result<Vec<Artifact>, Error> {
    c.query(
        "SELECT path, size, sha256 FROM artifacts WHERE job_id = $1 AND attempt = $2 ORDER BY path",
        &[job_id.into(), attempt.into()],
    )?
    .iter()
    .map(artifact_from_row)
    .collect()
}

fn artifact_from_row(row: &Row) -> Result<Artifact, Error> {
    Ok(Artifact {
        path: row.text(0)?.to_string(),
        size: row.int(1)?,
        sha256: row.text(2)?.to_string(),
    })
}

fn archived_release_from_row(row: &Row) -> Result<ArchivedRelease, Error> {
    Ok(ArchivedRelease {
        rosdistro: row.text(0)?.to_string(),
        repository: row.text(1)?.to_string(),
        version: row.text(2)?.to_string(),
        commit: row.text(3)?.to_string(),
        sha256: row.text(4)?.to_string(),
        record: row.text(5)?.to_string(),
    })
}

fn worker_from_row(row: &Row) -> Result<Worker, Error> {
    Ok(Worker {
        id: row.int(0)?,
        pool: row.text(1)?.to_string(),
        os: row.text(2)?.to_string(),
        arch: row.text(3)?.to_string(),
        draining: row.bool(4)?,
    })
}

fn no_row() -> Error {
    Error::Protocol("a statement that must return a row returned none".to_string())
}

fn migrate(c: &mut Connection) -> Result<(), Error> {
    c.batch("CREATE TABLE IF NOT EXISTS schema_migrations (version bigint PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())")?;
    let applied = c
        .query(
            "SELECT coalesce(max(version), 0) FROM schema_migrations",
            &[],
        )?
        .first()
        .ok_or_else(no_row)?
        .int(0)?;
    if applied > MIGRATIONS.len() as i64 {
        return Err(Error::Config(format!(
            "the database schema is at version {applied}, newer than this coordinator understands ({})",
            MIGRATIONS.len()
        )));
    }
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= applied {
            continue;
        }
        c.transaction(|c| {
            c.batch(sql)?;
            c.execute(
                "INSERT INTO schema_migrations (version) VALUES ($1)",
                &[Param::Int(version)],
            )?;
            Ok::<(), Error>(())
        })?;
        eprintln!("applied schema migration {version}");
    }
    Ok(())
}
