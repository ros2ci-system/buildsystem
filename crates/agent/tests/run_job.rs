// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Runs real jobs through the agent against a real container runtime.
//!
//! These tests need a runtime, so they only run when the
//! `CI_CONTAINER_RUNTIME` environment variable names one, for example
//! `unix:///var/run/docker.sock`. Without it they pass without doing
//! anything, and say so.
//!
//! On a Linux host the jobs use a small Linux image and `sh`. On a
//! Windows host they use a Windows image and `cmd`. Set `CI_TEST_IMAGE` to
//! use a different image.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// What differs between Linux and Windows containers: the image, the
/// shell, and the script for each test.
struct Platform {
    image: &'static str,
    shell: [&'static str; 2],
    job_dir: &'static str,
    success: &'static str,
    failure: &'static str,
    reserved: &'static str,
    timeout: &'static str,
    nothing: &'static str,
    large: &'static str,
}

#[cfg(not(windows))]
const PLATFORM: Platform = Platform {
    image: "docker.io/library/alpine:3.20",
    shell: ["sh", "-c"],
    job_dir: "/job",
    success: "echo on-stdout; echo on-stderr >&2; echo dir=$CI_JOB_DIR; \
              cat /job/params.json; cat /job/in/input.txt; \
              mkdir -p /job/out/sub; echo results > /job/out/results.json; \
              echo deep > /job/out/sub/file.txt",
    failure: "echo building; echo partial > /job/out/partial.txt; exit 3",
    reserved: "exit 75",
    timeout: "echo started; echo before > /job/out/before.txt; sleep 600",
    nothing: "true",
    large: "cp /job/in/big.bin /job/out/big.bin; \
            i=0; while [ $i -lt 20000 ]; do echo line $i; i=$((i+1)); done",
};

#[cfg(windows)]
const PLATFORM: Platform = Platform {
    image: "mcr.microsoft.com/windows/nanoserver:ltsc2025",
    shell: ["cmd", "/c"],
    job_dir: "C:\\job",
    success: "echo on-stdout& echo on-stderr 1>&2& echo dir=%CI_JOB_DIR%& \
              type C:\\job\\params.json& type C:\\job\\in\\input.txt& \
              mkdir C:\\job\\out\\sub& echo results> C:\\job\\out\\results.json& \
              echo deep> C:\\job\\out\\sub\\file.txt",
    failure: "echo building& echo partial> C:\\job\\out\\partial.txt& exit 3",
    reserved: "exit 75",
    timeout: "echo started& echo before> C:\\job\\out\\before.txt& ping -n 600 127.0.0.1 >NUL",
    nothing: "ver",
    large: "copy /b C:\\job\\in\\big.bin C:\\job\\out\\big.bin >NUL& \
            for /L %i in (0,1,19999) do @echo line %i",
};

struct Run {
    exit_status: i32,
    work_dir: PathBuf,
    outcome: ci_json::Value,
}

impl Run {
    fn outcome(&self) -> &str {
        self.outcome
            .get("outcome")
            .and_then(|v| v.as_str())
            .unwrap()
    }

    fn message(&self) -> &str {
        self.outcome
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.work_dir.join("log.txt")).unwrap_or_default()
    }

    fn outputs(&self) -> Vec<&str> {
        self.outcome
            .get("outputs")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect()
    }
}

fn runtime() -> Option<String> {
    match std::env::var("CI_CONTAINER_RUNTIME") {
        Ok(value) if !value.is_empty() => Some(value),
        _ => {
            eprintln!("skipped: CI_CONTAINER_RUNTIME is not set");
            None
        }
    }
}

fn image() -> String {
    std::env::var("CI_TEST_IMAGE").unwrap_or_else(|_| PLATFORM.image.to_string())
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ci_agent_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes a job description that runs `script` under the platform's shell.
fn shell_job(image: &str, script: &str, timeout: u64, inputs: &[(&str, &Path)]) -> String {
    use ci_json::Value;
    Value::object([
        ("image", Value::from(image)),
        (
            "command",
            Value::strings([PLATFORM.shell[0], PLATFORM.shell[1], script]),
        ),
        (
            "parameters",
            Value::object([("distribution", Value::from("rolling"))]),
        ),
        (
            "inputs",
            Value::Array(
                inputs
                    .iter()
                    .map(|(name, path)| {
                        Value::object([
                            ("name", Value::from(*name)),
                            ("url", Value::from(format!("file://{}", path.display()))),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("timeout_seconds", Value::Int(timeout as i64)),
    ])
    .to_json_pretty()
}

fn run(runtime: &str, name: &str, job: &str) -> Run {
    let dir = scratch(name);
    let job_path = dir.join("job.json");
    fs::write(&job_path, job).unwrap();
    let work_dir = dir.join("work");
    let status = Command::new(env!("CARGO_BIN_EXE_ci-agent"))
        .arg("run-job")
        .arg("--job")
        .arg(&job_path)
        .arg("--work-dir")
        .arg(&work_dir)
        .arg("--runtime")
        .arg(runtime)
        .status()
        .unwrap();
    let outcome =
        ci_json::parse(&fs::read_to_string(work_dir.join("outcome.json")).unwrap()).unwrap();
    assert!(
        !work_dir.join("upload.tar").exists(),
        "the upload archive must be cleaned up"
    );
    Run {
        exit_status: status.code().unwrap(),
        work_dir,
        outcome,
    }
}

#[test]
fn successful_job_gets_inputs_and_returns_outputs() {
    let Some(runtime) = runtime() else { return };
    let inputs = scratch("success_inputs");
    let input = inputs.join("input.txt");
    fs::write(&input, "from the input file\n").unwrap();
    let result = run(
        &runtime,
        "success",
        &shell_job(&image(), PLATFORM.success, 120, &[("input.txt", &input)]),
    );
    assert_eq!(
        result.outcome(),
        "success",
        "{}\n{}",
        result.message(),
        result.log()
    );
    assert_eq!(result.exit_status, 0);
    assert_eq!(result.outcome.get("exit_code").unwrap().as_i64(), Some(0));
    assert!(result
        .outcome
        .get("image_id")
        .unwrap()
        .as_str()
        .unwrap()
        .starts_with("sha256:"));

    let log = result.log();
    for expected in [
        "on-stdout".to_string(),
        "on-stderr".to_string(),
        format!("dir={}", PLATFORM.job_dir),
        "\"distribution\": \"rolling\"".to_string(),
        "from the input file".to_string(),
    ] {
        assert!(
            log.contains(&expected),
            "log is missing {expected:?}:\n{log}"
        );
    }

    let mut outputs = result.outputs();
    outputs.sort();
    assert_eq!(outputs, ["out/results.json", "out/sub/file.txt"]);
    assert_eq!(
        fs::read_to_string(result.work_dir.join("out/results.json"))
            .unwrap()
            .trim(),
        "results"
    );
    assert_eq!(
        fs::read_to_string(result.work_dir.join("out/sub/file.txt"))
            .unwrap()
            .trim(),
        "deep"
    );
}

#[test]
fn failing_job_is_a_build_failure_and_keeps_its_outputs() {
    let Some(runtime) = runtime() else { return };
    let result = run(
        &runtime,
        "failure",
        &shell_job(&image(), PLATFORM.failure, 120, &[]),
    );
    assert_eq!(result.outcome(), "build_failure");
    assert_eq!(result.exit_status, 1);
    assert_eq!(result.outcome.get("exit_code").unwrap().as_i64(), Some(3));
    assert_eq!(result.outputs(), ["out/partial.txt"]);
    assert!(result.log().contains("building"));
}

#[test]
fn reserved_exit_code_is_an_infrastructure_failure() {
    let Some(runtime) = runtime() else { return };
    let result = run(
        &runtime,
        "reserved",
        &shell_job(&image(), PLATFORM.reserved, 120, &[]),
    );
    assert_eq!(result.outcome(), "infrastructure_failure");
    assert_eq!(result.exit_status, 2);
    assert_eq!(result.outcome.get("exit_code").unwrap().as_i64(), Some(75));
}

#[test]
fn job_past_its_timeout_is_killed() {
    let Some(runtime) = runtime() else { return };
    let started = Instant::now();
    let result = run(
        &runtime,
        "timeout",
        &shell_job(&image(), PLATFORM.timeout, 3, &[]),
    );
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "the job should have been killed at its timeout"
    );
    assert_eq!(result.outcome(), "timeout", "{}", result.message());
    assert_eq!(result.exit_status, 1);
    assert!(result.outcome.get("exit_code").unwrap().is_null());
    assert_eq!(result.outputs(), ["out/before.txt"]);
    assert!(result.log().contains("started"));
}

/// A Linux container gets the 64 MiB of shared memory the runtime gives by
/// default, or the size the job asks for.
#[cfg(not(windows))]
#[test]
fn shared_memory_is_the_size_the_job_asks_for() {
    let Some(runtime) = runtime() else { return };
    let script = "df -m /dev/shm | tail -1";
    let size = |log: &str| -> u64 {
        let line = log.lines().rev().find(|l| l.contains("/dev/shm")).unwrap();
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    };

    let plain = run(
        &runtime,
        "shm_default",
        &shell_job(&image(), script, 120, &[]),
    );
    assert_eq!(plain.outcome(), "success", "{}", plain.message());
    assert!(size(&plain.log()) <= 64, "{}", plain.log());

    let job = shell_job(&image(), script, 120, &[]).replacen('{', "{\"shared_memory_mb\": 300,", 1);
    let sized = run(&runtime, "shm_sized", &job);
    assert_eq!(sized.outcome(), "success", "{}", sized.message());
    assert_eq!(size(&sized.log()), 300, "{}", sized.log());
}

#[test]
fn missing_image_is_an_infrastructure_failure() {
    let Some(runtime) = runtime() else { return };
    let job = shell_job(
        "localhost:1/ros2ci-no-such-image:1",
        PLATFORM.nothing,
        120,
        &[],
    );
    let result = run(&runtime, "missing_image", &job);
    assert_eq!(result.outcome(), "infrastructure_failure");
    assert_eq!(result.exit_status, 2);
    assert!(result.message().contains("image"), "{}", result.message());
}

#[test]
fn missing_input_is_an_infrastructure_failure() {
    let Some(runtime) = runtime() else { return };
    let missing = scratch("missing_input_source").join("not-there");
    let job = shell_job(&image(), PLATFORM.nothing, 120, &[("x", &missing)]);
    let result = run(&runtime, "missing_input", &job);
    assert_eq!(result.outcome(), "infrastructure_failure");
    assert_eq!(result.exit_status, 2);
    assert!(result.message().contains("input x"), "{}", result.message());
}

#[test]
fn large_files_and_logs_survive_intact() {
    let Some(runtime) = runtime() else { return };
    // 20 MB that does not compress or repeat goes in as an input and comes
    // back as an output, alongside 20,000 log lines.
    let inputs = scratch("large_inputs");
    let input = inputs.join("big.bin");
    let mut data = Vec::with_capacity(20_000_000);
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    while data.len() < 20_000_000 {
        // xorshift64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    fs::write(&input, &data).unwrap();

    let result = run(
        &runtime,
        "large",
        &shell_job(&image(), PLATFORM.large, 600, &[("big.bin", &input)]),
    );
    assert_eq!(result.outcome(), "success", "{}", result.message());

    let log = result.log();
    let lines: Vec<&str> = log.lines().map(str::trim_end).collect();
    assert_eq!(lines.len(), 20000);
    assert_eq!(lines[0], "line 0");
    assert_eq!(lines[19999], "line 19999");

    let returned = fs::read(result.work_dir.join("out/big.bin")).unwrap();
    assert_eq!(returned.len(), data.len());
    assert!(
        returned == data,
        "the file changed on its way through the container"
    );
}
