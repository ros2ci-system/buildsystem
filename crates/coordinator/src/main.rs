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

  --data-dir <dir>           Where logs and artifacts are stored.
  --worker-listen <addr>     Address for the worker API. Default 0.0.0.0:8443.
  --admin-listen <addr>      Address for the admin API. Default 127.0.0.1:8444.
                             The admin API has no authentication: keep this
                             address private.
  --tls-cert <file>          Certificate chain for the worker API, in PEM.
  --tls-key <file>           Its private key, in PEM. Without these two the
                             worker API is plain HTTP, for local use only.
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
    settings_numbers: [i64; 5],
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut data_dir = None;
    let mut worker_listen = "0.0.0.0:8443".to_string();
    let mut admin_listen = "127.0.0.1:8444".to_string();
    let mut tls_cert = None;
    let mut tls_key = None;
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

    let [lease, report, claim, max_failures, backoff] = options.settings_numbers;
    let coordinator = Arc::new(Coordinator {
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
