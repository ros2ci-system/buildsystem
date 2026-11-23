// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Runs one job in a container.
//!
//! The contract between the agent and a job, which is the same for every
//! kind of job:
//!
//! - **Job directory.** `/job` on Linux and `C:\job` on Windows. Its path
//!   is also in the `CI_JOB_DIR` environment variable.
//! - **Parameters.** `params.json` in the job directory.
//! - **Inputs.** Files in the `in` subdirectory.
//! - **Outputs.** Whatever the job writes to the `out` subdirectory is
//!   copied out when it finishes. Results go in `out/results.json`.
//! - **Exit code.** Zero is success. [`EXIT_INFRASTRUCTURE`] means a
//!   problem that was not the fault of the code being built, such as a
//!   package mirror being down. Anything else is a build failure.
//!
//! Files go in and out through the container runtime's API. Nothing is
//! mounted from the host, so the agent can itself run in a container.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ci_container::{Client, ContainerSpec};
use ci_json::Value;

use ci_net::{Connector, Url};
use ci_protocol::{InputSource, JobDescription, Outcome};

/// The exit code a job uses to say "this failed for a reason outside the
/// code under test; try again". It is `EX_TEMPFAIL` from `sysexits.h`.
pub const EXIT_INFRASTRUCTURE: i64 = 75;

/// The label that marks a container as belonging to one agent, so that
/// the agent can find and remove its containers after a crash.
pub const OWNER_LABEL: &str = "ros2ci.agent";

/// How often the container's state is checked while it runs.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// What the caller wants done with a running job. The agent asks at
/// short intervals, which is also the caller's chance to do periodic
/// work of its own, such as sending the log so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Continue,
    /// Stop the job as soon as possible.
    Stop,
}

#[derive(Debug, Clone)]
pub struct JobResult {
    pub outcome: Outcome,
    /// The container's exit code, when it ran to an exit.
    pub exit_code: Option<i64>,
    /// Why, for an infrastructure failure.
    pub message: String,
    /// The id of the image the container actually ran.
    pub image_id: String,
    pub duration: Duration,
    /// Output files, relative to the work directory.
    pub outputs: Vec<PathBuf>,
}

impl JobResult {
    pub fn to_json(&self) -> Value {
        Value::object([
            ("outcome", Value::from(self.outcome.as_str())),
            (
                "exit_code",
                self.exit_code.map(Value::Int).unwrap_or(Value::Null),
            ),
            ("message", Value::from(self.message.as_str())),
            ("image_id", Value::from(self.image_id.as_str())),
            (
                "duration_seconds",
                Value::Float((self.duration.as_secs_f64() * 1000.0).round() / 1000.0),
            ),
            (
                "outputs",
                Value::strings(
                    self.outputs
                        .iter()
                        .map(|p| p.to_string_lossy().replace('\\', "/")),
                ),
            ),
        ])
    }
}

/// Where the job directory is inside the container.
struct Layout {
    /// The directory the upload archive is unpacked in.
    root: &'static str,
    job_dir: &'static str,
    out_dir: &'static str,
}

const LINUX: Layout = Layout {
    root: "/",
    job_dir: "/job",
    out_dir: "/job/out",
};

const WINDOWS: Layout = Layout {
    root: "C:\\",
    job_dir: "C:\\job",
    out_dir: "C:\\job\\out",
};

/// Removes the container when dropped, so no path through the code can
/// leave one behind.
struct ContainerGuard<'a> {
    client: &'a Client,
    id: String,
}

impl Drop for ContainerGuard<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.client.remove_container(&self.id) {
            eprintln!("warning: could not remove container {}: {e}", self.id);
        }
    }
}

/// Runs the job and writes its log to `log.txt` and its outputs to `out/`
/// in `work_dir`. Problems with the runtime, the image or the inputs come
/// back as an infrastructure failure, not as an error: the only errors
/// are ones that stop the agent from recording anything at all.
pub fn run_job(
    client: &Client,
    connector: &Connector,
    job: &JobDescription,
    work_dir: &Path,
    owner: Option<&str>,
    monitor: &mut dyn FnMut() -> Control,
) -> io::Result<JobResult> {
    fs::create_dir_all(work_dir)?;
    let started = Instant::now();
    let mut collected = Collected::default();
    let (outcome, exit_code, message) = match run_in_container(
        client,
        connector,
        job,
        work_dir,
        owner,
        monitor,
        &mut collected,
    ) {
        Ok(Finished::Exited(0)) => (Outcome::Success, Some(0), String::new()),
        Ok(Finished::Exited(EXIT_INFRASTRUCTURE)) => (
            Outcome::InfrastructureFailure,
            Some(EXIT_INFRASTRUCTURE),
            "the job reported an infrastructure problem".to_string(),
        ),
        Ok(Finished::Exited(code)) => (Outcome::BuildFailure, Some(code), String::new()),
        Ok(Finished::TimedOut) => (
            Outcome::Timeout,
            None,
            format!("killed after the {} second timeout", job.timeout.as_secs()),
        ),
        Ok(Finished::Stopped) => (Outcome::Cancelled, None, "stopped on request".to_string()),
        Err(message) => (Outcome::InfrastructureFailure, None, message),
    };
    Ok(JobResult {
        outcome,
        exit_code,
        message,
        image_id: collected.image_id,
        duration: started.elapsed(),
        outputs: collected.outputs,
    })
}

/// What is known about a run whether or not it ended well.
#[derive(Default)]
struct Collected {
    /// The id of the image the container ran.
    image_id: String,
    /// Output files, relative to the work directory.
    outputs: Vec<PathBuf>,
}

enum Finished {
    Exited(i64),
    TimedOut,
    /// The caller asked for the job to be stopped.
    Stopped,
}

/// Every error from here is an infrastructure failure; the string says
/// what went wrong.
fn run_in_container(
    client: &Client,
    connector: &Connector,
    job: &JobDescription,
    work_dir: &Path,
    owner: Option<&str>,
    monitor: &mut dyn FnMut() -> Control,
    collected: &mut Collected,
) -> Result<Finished, String> {
    let version = client.version().map_err(|e| e.to_string())?;
    let windows = version
        .get("Os")
        .and_then(Value::as_str)
        .is_some_and(|os| os.eq_ignore_ascii_case("windows"));
    let layout = if windows { &WINDOWS } else { &LINUX };

    // Images are named by digest, so one that is already present is the
    // right one and there is nothing to gain from asking the registry.
    let present = client
        .image_exists(&job.image)
        .map_err(|e| format!("checking for image {}: {e}", job.image))?;
    if !present {
        eprintln!("pulling {}", job.image);
        // A pull can take many minutes. The monitor is called as progress
        // arrives, so the caller is not left silent for the whole of it.
        let mut stop = false;
        client
            .pull_image(&job.image, |_| stop |= monitor() == Control::Stop)
            .map_err(|e| format!("pulling image {}: {e}", job.image))?;
        if stop {
            return Ok(Finished::Stopped);
        }
    }

    // Inputs are fetched before the container exists, so a download that
    // fails costs nothing to clean up.
    let downloads = work_dir.join("inputs");
    for input in &job.inputs {
        if let InputSource::Url(url) = &input.source {
            fs::create_dir_all(&downloads)
                .map_err(|e| format!("preparing the job's inputs: {e}"))?;
            let stopped = download(connector, url, &downloads.join(&input.name), monitor)
                .map_err(|e| format!("input {}: {e}", input.name))?;
            if stopped {
                return Ok(Finished::Stopped);
            }
        }
    }

    let mut environment = job.environment.clone();
    environment.retain(|(name, _)| name != "CI_JOB_DIR");
    environment.push(("CI_JOB_DIR".to_string(), layout.job_dir.to_string()));
    let spec = ContainerSpec {
        image: job.image.clone(),
        command: job.command.clone(),
        environment,
        working_dir: None,
        // Windows 11 defaults to Hyper-V isolation, which needs nested
        // virtualization on a virtual machine. Process isolation does not.
        isolation: windows.then(|| "process".to_string()),
        labels: owner
            .map(|owner| vec![(OWNER_LABEL.to_string(), owner.to_string())])
            .unwrap_or_default(),
    };
    let id = client
        .create_container(&spec)
        .map_err(|e| format!("creating the container: {e}"))?;
    let guard = ContainerGuard { client, id };
    let id = guard.id.as_str();

    // The inputs can be large, so the archive is built on disk and
    // streamed from there.
    let archive_path = work_dir.join("upload.tar");
    let uploaded = build_upload_archive(job, &downloads, &archive_path)
        .map_err(|e| format!("preparing the job's inputs: {e}"))
        .and_then(|()| {
            let mut archive = File::open(&archive_path)
                .map_err(|e| format!("preparing the job's inputs: {e}"))?;
            client
                .upload_archive(id, layout.root, &mut archive)
                .map_err(|e| format!("copying inputs into the container: {e}"))
        });
    let _ = fs::remove_file(&archive_path);
    let _ = fs::remove_dir_all(&downloads);
    uploaded?;

    let log_path = work_dir.join("log.txt");
    let log_file = File::create(&log_path).map_err(|e| format!("creating the log file: {e}"))?;

    client
        .start_container(id)
        .map_err(|e| format!("starting the container: {e}"))?;
    let deadline = Instant::now() + job.timeout;

    let mut logs = client
        .follow_logs(id)
        .map_err(|e| format!("following the container's log: {e}"))?;
    let log_thread = thread::spawn(move || -> Result<(), String> {
        let mut out = BufWriter::new(log_file);
        let mut frame = Vec::new();
        loop {
            match logs.next_frame(&mut frame) {
                Ok(Some(_source)) => {
                    out.write_all(&frame)
                        .map_err(|e| format!("writing the log: {e}"))?;
                    // Flushed per frame so the log on disk is current
                    // while the job runs.
                    out.flush().map_err(|e| format!("writing the log: {e}"))?;
                }
                Ok(None) => return Ok(()),
                Err(e) => return Err(format!("reading the container's log: {e}")),
            }
        }
    });

    let mut timed_out = false;
    let mut stopped = false;
    let state = loop {
        let state = client
            .inspect_container(id)
            .map_err(|e| format!("checking the container: {e}"))?;
        if !state.running {
            break state;
        }
        if !timed_out && !stopped {
            if monitor() == Control::Stop {
                stopped = true;
                client
                    .kill_container(id)
                    .map_err(|e| format!("killing the container on request: {e}"))?;
            } else if Instant::now() >= deadline {
                timed_out = true;
                client
                    .kill_container(id)
                    .map_err(|e| format!("killing the container at its timeout: {e}"))?;
            }
        }
        thread::sleep(POLL_INTERVAL);
    };
    collected.image_id = state.image_id.clone();

    // The log stream ends when the container stops.
    match log_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(message)) => return Err(message),
        Err(_) => return Err("the log thread panicked".to_string()),
    }

    // Outputs are collected whatever the outcome: a failed build's logs
    // and partial results are the ones people most want to see.
    let mut archive = client
        .download_archive(id, layout.out_dir)
        .map_err(|e| format!("copying outputs out of the container: {e}"))?;
    collected.outputs = ci_tar::Archive::new(&mut archive)
        .unpack(work_dir)
        .map_err(|e| format!("copying outputs out of the container: {e}"))?;

    if stopped {
        return Ok(Finished::Stopped);
    }
    if timed_out {
        return Ok(Finished::TimedOut);
    }
    if !state.error.is_empty() {
        return Err(format!("the container runtime reported: {}", state.error));
    }
    if state.oom_killed {
        return Err("the container was killed for running out of memory".to_string());
    }
    Ok(Finished::Exited(state.exit_code))
}

/// Builds the archive that sets up the job directory: the parameters
/// file, the inputs, and an empty output directory.
fn build_upload_archive(job: &JobDescription, downloads: &Path, path: &Path) -> Result<(), String> {
    let file = File::create(path).map_err(|e| e.to_string())?;
    let mut builder = ci_tar::Builder::new(BufWriter::new(file));
    let err = |e: ci_tar::Error| e.to_string();
    builder.append_dir("job", 0o755).map_err(err)?;
    builder
        .append_file(
            "job/params.json",
            0o644,
            job.parameters.to_json_pretty().as_bytes(),
        )
        .map_err(err)?;
    builder.append_dir("job/in", 0o755).map_err(err)?;
    for input in &job.inputs {
        let source = match &input.source {
            InputSource::LocalFile(source) => source.clone(),
            InputSource::Url(_) => downloads.join(&input.name),
        };
        let mut file = File::open(&source)
            .map_err(|e| format!("input {}: {}: {e}", input.name, source.display()))?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file() {
            return Err(format!(
                "input {}: {} is not a file",
                input.name,
                source.display()
            ));
        }
        builder
            .append_reader(
                &format!("job/in/{}", input.name),
                0o644,
                meta.len(),
                &mut file,
            )
            .map_err(err)?;
    }
    // World-writable, so a job that does not run as root can write here.
    builder.append_dir("job/out", 0o777).map_err(err)?;
    builder
        .finish()
        .map_err(err)?
        .into_inner()
        .map_err(|e| e.to_string())?
        .sync_all()
        .map_err(|e| e.to_string())
}

/// Downloads a URL to a file. Returns `true` if the caller asked for the
/// job to be stopped part-way.
fn download(
    connector: &Connector,
    url: &str,
    path: &Path,
    monitor: &mut dyn FnMut() -> Control,
) -> Result<bool, String> {
    let parsed = Url::parse(url).map_err(|e| e.to_string())?;
    let host = parsed.host_header();
    let mut response = connector
        .send(&parsed, ci_http::Request::new("GET", &parsed.target, &host))
        .map_err(|e| format!("downloading {url}: {e}"))?;
    if response.status != 200 {
        return Err(format!(
            "downloading {url}: the server answered {}",
            response.status
        ));
    }
    let mut file = File::create(path).map_err(|e| format!("saving {url}: {e}"))?;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = match response.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("downloading {url}: {e}")),
        };
        file.write_all(&buffer[..n])
            .map_err(|e| format!("saving {url}: {e}"))?;
        if monitor() == Control::Stop {
            return Ok(true);
        }
    }
    file.flush().map_err(|e| format!("saving {url}: {e}"))?;
    Ok(false)
}
