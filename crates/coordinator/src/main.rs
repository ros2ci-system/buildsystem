// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The coordinator: holds the job queue in PostgreSQL and hands jobs to
//! workers.
//!
//! It keeps no state of its own. Everything is in the database or in the
//! data directory, so stopping and starting it delays jobs but loses
//! nothing.

mod api;
mod db;
mod gitrefs;
mod repo;

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use api::{Coordinator, Settings};
use ci_net::{Acceptor, Stream};

const USAGE: &str = "\
Usage: ci-coordinator --data-dir <dir> [options]

The database is named by the CI_DATABASE_URL environment variable, as
postgres://user:password@host:port/database. It is not a command-line
option, so that the password does not show in the process list.

  --data-dir <dir>           Where logs and artifacts are stored. On first
                             start, the first admin token is written here as
                             the file admin-token.
  --worker-listen <addr>     Address for the worker API. Default 0.0.0.0:8443.
  --admin-listen <addr>      Address for the admin API. Default 127.0.0.1:8444.
                             Every call needs an admin token. Keep this
                             address private all the same.
  --tls-cert <file>          Certificate chain for the worker API, in PEM.
  --tls-key <file>           Its private key, in PEM. Without these two the
                             worker API is plain HTTP, for local use only.
  --repo-listen <addr>       Address to serve the package repositories on,
                             read-only, over plain HTTP. Their indexes are
                             signed, which is what clients check. Not
                             served unless given.
  --signing-key <file>       The key that signs repository indexes, as
                             the ci-admin generate-signing-key command writes it.
                             Without it nothing can be published.
  --reconcile-interval-seconds <n>
                             How often the reconciler takes a step.
                             Default 5.
  --lease-seconds <n>        How long a job stays claimed without a Report.
                             Default 60.
  --report-interval-seconds <n>
                             How often workers report. Default 10.
  --claim-interval-seconds <n>
                             How often idle workers ask for a job. Default 3.
  --max-infrastructure-failures <n>
                             Give up on a job after this many. Default 3.
  --retry-backoff-seconds <n>
                             Delay before the first retry; it doubles each
                             time. Default 30.
";

struct Options {
    data_dir: PathBuf,
    worker_listen: String,
    admin_listen: String,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    repo_listen: Option<String>,
    signing_key: Option<PathBuf>,
    settings_numbers: [i64; 5],
    reconcile_interval: u64,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut data_dir = None;
    let mut worker_listen = "0.0.0.0:8443".to_string();
    let mut admin_listen = "127.0.0.1:8444".to_string();
    let mut tls_cert = None;
    let mut tls_key = None;
    let mut repo_listen = None;
    let mut signing_key = None;
    let mut reconcile_interval = 5;
    // lease, report interval, claim interval, max failures, backoff
    let mut numbers = [60i64, 10, 3, 3, 30];
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = || {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        let number = |text: String| {
            text.parse::<i64>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| format!("{arg} needs a positive whole number"))
        };
        match arg.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
            "--worker-listen" => worker_listen = value()?,
            "--admin-listen" => admin_listen = value()?,
            "--tls-cert" => tls_cert = Some(PathBuf::from(value()?)),
            "--tls-key" => tls_key = Some(PathBuf::from(value()?)),
            "--repo-listen" => repo_listen = Some(value()?),
            "--signing-key" => signing_key = Some(PathBuf::from(value()?)),
            "--reconcile-interval-seconds" => reconcile_interval = number(value()?)? as u64,
            "--lease-seconds" => numbers[0] = number(value()?)?,
            "--report-interval-seconds" => numbers[1] = number(value()?)?,
            "--claim-interval-seconds" => numbers[2] = number(value()?)?,
            "--max-infrastructure-failures" => numbers[3] = number(value()?)?,
            "--retry-backoff-seconds" => numbers[4] = number(value()?)?,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if tls_cert.is_some() != tls_key.is_some() {
        return Err("--tls-cert and --tls-key must be given together".to_string());
    }
    if numbers[1] * 2 > numbers[0] {
        return Err("--lease-seconds must be at least twice --report-interval-seconds".to_string());
    }
    Ok(Options {
        data_dir: data_dir.ok_or("--data-dir is required")?,
        worker_listen,
        admin_listen,
        tls_cert,
        tls_key,
        repo_listen,
        signing_key,
        reconcile_interval,
        settings_numbers: numbers,
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("ci-coordinator: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let options = parse_options(args)?;
    let database_url = std::env::var("CI_DATABASE_URL")
        .map_err(|_| "the CI_DATABASE_URL environment variable is not set".to_string())?;
    let database = ci_postgres::Config::parse(&database_url).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&options.data_dir)
        .map_err(|e| format!("cannot create {}: {e}", options.data_dir.display()))?;

    let acceptor = match (&options.tls_cert, &options.tls_key) {
        (Some(cert), Some(key)) => {
            Some(Acceptor::from_pem_files(cert, key).map_err(|e| e.to_string())?)
        }
        _ => None,
    };

    let signing_key = match &options.signing_key {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let key = ci_openpgp::SigningKey::from_file_text(&text)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            eprintln!(
                "repository indexes are signed by {} ({})",
                key.fingerprint_hex(),
                key.user_id()
            );
            Some(key)
        }
        None => None,
    };

    let [lease, report, claim, max_failures, backoff] = options.settings_numbers;
    let coordinator = Arc::new(Coordinator {
        signing_key,
        publishing: std::sync::Mutex::new(()),
        reconciling: std::sync::Mutex::new(()),
        reconcile_millis: std::sync::atomic::AtomicU64::new(0),
        forges: ci_net::Connector::with_public_roots(None).map_err(|e| e.to_string())?,
        db: db::Db::open(database)?,
        settings: Settings {
            data_dir: options.data_dir.clone(),
            lease_seconds: lease,
            report_interval_seconds: report,
            claim_interval_seconds: claim,
            max_infrastructure_failures: max_failures,
            retry_backoff_seconds: backoff,
            max_artifact_bytes: 16 * 1024 * 1024 * 1024,
        },
    });

    // With no admin token, nobody could use the admin API to create one.
    // The first is written to a file that only this user can read.
    if let Some(token) = coordinator
        .bootstrap_admin_token()
        .map_err(|e| e.to_string())?
    {
        let path = options.data_dir.join("admin-token");
        write_private_file(&path, &token)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        eprintln!(
            "created the first admin token and wrote it to {}",
            path.display()
        );
    }

    // A stop between publishing a build and writing the index left the
    // index behind what the repository holds.
    coordinator.write_stale_indexes()?;

    // Time spent down must not count against workers that could not
    // reach the coordinator.
    let extended = coordinator
        .db
        .extend_all_leases(lease)
        .map_err(|e| e.to_string())?;
    if extended > 0 {
        eprintln!("extended the lease of {extended} running job(s)");
    }

    let worker_listener = TcpListener::bind(&options.worker_listen)
        .map_err(|e| format!("cannot listen on {}: {e}", options.worker_listen))?;
    let admin_listener = TcpListener::bind(&options.admin_listen)
        .map_err(|e| format!("cannot listen on {}: {e}", options.admin_listen))?;
    eprintln!(
        "worker API on {} ({}), admin API on {}",
        worker_listener.local_addr().map_err(|e| e.to_string())?,
        if acceptor.is_some() {
            "TLS"
        } else {
            "plain HTTP, for local use only"
        },
        admin_listener.local_addr().map_err(|e| e.to_string())?,
    );

    {
        let coordinator = Arc::clone(&coordinator);
        thread::spawn(move || {
            ci_http::server::serve(
                admin_listener,
                4,
                |socket| Ok(Stream::Plain(socket)),
                move |request| coordinator.handle_admin(request),
            )
        });
    }
    {
        // The reconciler does nothing until it is given settings.
        let coordinator = Arc::clone(&coordinator);
        let interval = Duration::from_secs(options.reconcile_interval);
        thread::spawn(move || {
            let mut last_error = String::new();
            loop {
                thread::sleep(interval);
                let started = std::time::Instant::now();
                let result = coordinator.reconcile();
                coordinator.reconcile_millis.store(
                    started.elapsed().as_millis() as u64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                match result {
                    Ok(()) => last_error.clear(),
                    // The same trouble every five seconds is said once.
                    Err(message) if message != last_error => {
                        eprintln!("reconciler: {message}");
                        last_error = message;
                    }
                    Err(_) => {}
                }
            }
        });
    }
    if let Some(address) = &options.repo_listen {
        let listener =
            TcpListener::bind(address).map_err(|e| format!("cannot listen on {address}: {e}"))?;
        eprintln!(
            "package repositories on {} (plain HTTP, read-only)",
            listener.local_addr().map_err(|e| e.to_string())?
        );
        let coordinator = Arc::clone(&coordinator);
        thread::spawn(move || {
            ci_http::server::serve(
                listener,
                16,
                |socket| Ok(Stream::Plain(socket)),
                move |request| coordinator.handle_repository(request),
            )
        });
    }
    {
        let coordinator = Arc::clone(&coordinator);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(2));
            match coordinator.db.expire_leases(max_failures, backoff) {
                Ok(expired) => {
                    for (job_id, result) in expired {
                        eprintln!("job {job_id}: the worker stopped reporting; {result:?}");
                    }
                }
                Err(e) => eprintln!("error: checking leases: {e}"),
            }
        });
    }

    let error = ci_http::server::serve(
        worker_listener,
        16,
        move |socket| match &acceptor {
            Some(acceptor) => acceptor.accept(socket),
            None => Ok(Stream::Plain(socket)),
        },
        move |request| coordinator.handle_worker(request),
    );
    Err(format!("the worker API stopped: {error}"))
}

/// Writes a file that only the current user can read. On Unix the
/// permissions are set when the file is created, so there is no moment at
/// which others could read it. On Windows the file takes the permissions
/// of its directory, so the data directory itself must be private.
fn write_private_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // A leftover file from an earlier database is replaced, never
    // appended to or reused.
    let _ = std::fs::remove_file(path);
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}
