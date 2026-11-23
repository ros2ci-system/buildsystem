// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The worker agent.
//!
//! For now it has one command, `run-job`, which runs a single job from a
//! job description file. Talking to the coordinator comes later; it will
//! hand the same job descriptions to the same code.

mod job;
mod run;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ci_container::{Client, Endpoint};

use job::JobDescription;
use run::Outcome;

const USAGE: &str = "\
Usage: ci-agent run-job --job <file> --work-dir <dir> [--runtime <endpoint>]

Runs one job in a container and leaves log.txt, outcome.json and the job's
outputs (under out/) in the work directory.

  --job <file>          The job description, a JSON file.
  --work-dir <dir>      Where to put the log and outputs. Created if missing.
  --runtime <endpoint>  The container runtime, as unix:///path/to/socket or
                        npipe:////./pipe/name. Defaults to the
                        CI_CONTAINER_RUNTIME environment variable, then to
                        the usual Docker or Podman location.

Exit status: 0 the job succeeded, 1 it failed or timed out, 2 it could not
be run (an infrastructure failure), 64 bad usage.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run-job") => match run_job_command(&args[1..]) {
            Ok(code) => code,
            Err(message) => {
                eprintln!("ci-agent: {message}");
                ExitCode::from(64)
            }
        },
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(64)
        }
    }
}

fn run_job_command(args: &[String]) -> Result<ExitCode, String> {
    let mut job_path: Option<PathBuf> = None;
    let mut work_dir: Option<PathBuf> = None;
    let mut runtime: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--job" => job_path = Some(PathBuf::from(value("--job")?)),
            "--work-dir" => work_dir = Some(PathBuf::from(value("--work-dir")?)),
            "--runtime" => runtime = Some(value("--runtime")?),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let job_path = job_path.ok_or("--job is required")?;
    let work_dir = work_dir.ok_or("--work-dir is required")?;

    let text = fs::read_to_string(&job_path)
        .map_err(|e| format!("cannot read {}: {e}", job_path.display()))?;
    let job = JobDescription::parse(&text).map_err(|e| format!("{}: {e}", job_path.display()))?;

    let endpoint = match runtime.or_else(|| std::env::var("CI_CONTAINER_RUNTIME").ok()) {
        Some(text) => Endpoint::parse(&text).map_err(|e| e.to_string())?,
        None => default_endpoint()?,
    };
    let client = Client::new(endpoint);

    let result = run::run_job(&client, &job, &work_dir)
        .map_err(|e| format!("cannot use work directory {}: {e}", work_dir.display()))?;
    let outcome_path = work_dir.join("outcome.json");
    fs::write(&outcome_path, result.to_json().to_json_pretty())
        .map_err(|e| format!("cannot write {}: {e}", outcome_path.display()))?;

    eprintln!(
        "{}{}",
        result.outcome.as_str(),
        if result.message.is_empty() {
            String::new()
        } else {
            format!(": {}", result.message)
        }
    );
    Ok(match result.outcome {
        Outcome::Success => ExitCode::SUCCESS,
        Outcome::BuildFailure | Outcome::Timeout => ExitCode::from(1),
        Outcome::InfrastructureFailure => ExitCode::from(2),
    })
}

#[cfg(unix)]
fn default_endpoint() -> Result<Endpoint, String> {
    let mut candidates = vec![PathBuf::from("/var/run/docker.sock")];
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        candidates.push(Path::new(&runtime_dir).join("podman/podman.sock"));
    }
    candidates.push(PathBuf::from("/run/podman/podman.sock"));
    candidates
        .into_iter()
        .find(|path| path.exists())
        .map(Endpoint::Unix)
        .ok_or_else(|| {
            "no container runtime socket found; use --runtime or CI_CONTAINER_RUNTIME".to_string()
        })
}

#[cfg(windows)]
fn default_endpoint() -> Result<Endpoint, String> {
    let _ = Path::new("");
    Ok(Endpoint::NamedPipe(
        "\\\\.\\pipe\\docker_engine".to_string(),
    ))
}
