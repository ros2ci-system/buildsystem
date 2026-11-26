// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The worker agent.
//!
//! For now it has one command, `run-job`, which runs a single job from a
//! job description file. Talking to the coordinator comes later; it will
//! hand the same job descriptions to the same code.

mod run;
mod worker;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ci_container::{Client, Endpoint};

use ci_protocol::{JobDescription, Outcome};

const USAGE: &str = "\
Usage:
  ci-agent work --coordinator <url> --token-file <file> --work-dir <dir>
                [--ca-file <file>] [--runtime <endpoint>] [--job-network <name>]
  ci-agent run-job --job <file> --work-dir <dir> [--runtime <endpoint>]
                   [--job-network <name>]

work: registers with a coordinator, then claims and runs jobs until killed.

  --coordinator <url>   The coordinator's worker API, as https://host:port.
                        http:// is accepted for local use.
  --token-file <file>   A file holding this worker's enrolment token. The
                        token decides which pool the worker serves.
  --ca-file <file>      Certificate authorities to trust for https://, in
                        PEM. Required for an https:// coordinator.
  --work-dir <dir>      Scratch space for running jobs. Created if missing.

run-job: runs one job from a file, with no coordinator, and leaves log.txt,
outcome.json and the job's outputs (under out/) in the work directory.

  --job <file>          The job description, a JSON file.
  --work-dir <dir>      Where to put the log and outputs. Created if missing.

Both:

  --runtime <endpoint>  The container runtime, as unix:///path/to/socket or
                        npipe:////./pipe/name. Defaults to the
                        CI_CONTAINER_RUNTIME environment variable, then to
                        the usual Docker or Podman location.
  --job-network <name>  A network of the container runtime for job
                        containers to join, instead of its default. Use
                        it when jobs must reach a service that is only on
                        that network, such as a coordinator in a container
                        serving package repositories. The name host shares this
                        machine's network.

Exit status of run-job: 0 the job succeeded, 1 it failed or timed out, 2 it
could not be run (an infrastructure failure), 64 bad usage.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("work") => match work_command(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("ci-agent: {message}");
                ExitCode::FAILURE
            }
        },
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

fn runtime_client(runtime: Option<String>, network: Option<String>) -> Result<Client, String> {
    let endpoint = match runtime.or_else(|| std::env::var("CI_CONTAINER_RUNTIME").ok()) {
        Some(text) => Endpoint::parse(&text).map_err(|e| e.to_string())?,
        None => default_endpoint()?,
    };
    let client = Client::new(endpoint);
    match network {
        Some(network) => client
            .with_network(&network)
            .map_err(|e| format!("--job-network: {e}")),
        None => Ok(client),
    }
}

fn work_command(args: &[String]) -> Result<(), String> {
    let mut coordinator: Option<String> = None;
    let mut token_file: Option<PathBuf> = None;
    let mut ca_file: Option<PathBuf> = None;
    let mut work_dir: Option<PathBuf> = None;
    let mut runtime: Option<String> = None;
    let mut network: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = || {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--coordinator" => coordinator = Some(value()?),
            "--token-file" => token_file = Some(PathBuf::from(value()?)),
            "--ca-file" => ca_file = Some(PathBuf::from(value()?)),
            "--work-dir" => work_dir = Some(PathBuf::from(value()?)),
            "--runtime" => runtime = Some(value()?),
            "--job-network" => network = Some(value()?),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let coordinator = ci_net::Url::parse(&coordinator.ok_or("--coordinator is required")?)
        .map_err(|e| e.to_string())?;
    if coordinator.target != "/" {
        return Err("--coordinator must be a bare address, such as https://host:8443".to_string());
    }
    let token_file = token_file.ok_or("--token-file is required")?;
    let enrolment_token = fs::read_to_string(&token_file)
        .map_err(|e| format!("cannot read {}: {e}", token_file.display()))?
        .trim()
        .to_string();
    if enrolment_token.is_empty() {
        return Err(format!("{} is empty", token_file.display()));
    }
    let connector = match &ca_file {
        Some(path) => ci_net::Connector::with_ca_file(path).map_err(|e| e.to_string())?,
        None if coordinator.https => {
            return Err("an https:// coordinator needs --ca-file".to_string());
        }
        None => {
            eprintln!("warning: talking to the coordinator over plain HTTP; use this only locally");
            ci_net::Connector::plain_only()
        }
    };
    // Job inputs come from public servers, whose certificates the usual
    // public authorities vouch for. The coordinator is trusted only
    // through the file given for it.
    let downloads =
        ci_net::Connector::with_public_roots(ca_file.as_deref()).map_err(|e| e.to_string())?;
    worker::Worker {
        coordinator,
        connector,
        downloads,
        enrolment_token,
        runtime: runtime_client(runtime, network)?,
        work_dir: work_dir.ok_or("--work-dir is required")?,
    }
    .run()
}

fn run_job_command(args: &[String]) -> Result<ExitCode, String> {
    let mut job_path: Option<PathBuf> = None;
    let mut work_dir: Option<PathBuf> = None;
    let mut runtime: Option<String> = None;
    let mut network: Option<String> = None;
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
            "--job-network" => network = Some(value("--job-network")?),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let job_path = job_path.ok_or("--job is required")?;
    let work_dir = work_dir.ok_or("--work-dir is required")?;

    let text = fs::read_to_string(&job_path)
        .map_err(|e| format!("cannot read {}: {e}", job_path.display()))?;
    let job = JobDescription::parse(&text).map_err(|e| format!("{}: {e}", job_path.display()))?;

    let client = runtime_client(runtime, network)?;

    // A job run by hand has nobody to report to and nobody to stop it.
    let result = run::run_job(
        &client,
        &run::Fetcher {
            connector: &ci_net::Connector::plain_only(),
            downloads: &ci_net::Connector::with_public_roots(None).map_err(|e| e.to_string())?,
            stored: None,
        },
        &job,
        &work_dir,
        None,
        &mut || run::Control::Continue,
    )
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
        Outcome::BuildFailure | Outcome::Timeout | Outcome::Cancelled => ExitCode::from(1),
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
