// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Runs real jobs through the agent against a real container runtime.
//!
//! These tests need a runtime, so they only run when the
//! `CI_CONTAINER_RUNTIME` environment variable names one, for example
//! `unix:///var/run/docker.sock`. Without it they pass without doing
//! anything, and say so.
//!
//! The jobs use a small Linux image and `sh`. Set `CI_TEST_IMAGE` to use a
//! different image. Windows containers need their own jobs, which are not
//! written yet.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const DEFAULT_IMAGE: &str = "docker.io/library/alpine:3.20";

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
    std::env::var("CI_TEST_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string())
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ci_agent_{}_{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes a job description that runs `script` under `sh -c`.
fn shell_job(image: &str, script: &str, timeout: u64, inputs: &[(&str, &Path)]) -> String {
    use ci_json::Value;
    Value::object([
        ("image", Value::from(image)),
        ("command", Value::strings(["sh", "-c", script])),
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
    let script = "echo on-stdout; echo on-stderr >&2; echo dir=$CI_JOB_DIR; \
                  cat /job/params.json; cat /job/in/input.txt; \
                  mkdir -p /job/out/sub; echo '{\"tests\": []}' > /job/out/results.json; \
                  echo deep > /job/out/sub/file.txt";
    let result = run(
        &runtime,
        "success",
        &shell_job(&image(), script, 120, &[("input.txt", &input)]),
    );
    assert_eq!(result.outcome(), "success", "{}", result.message());
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
        "on-stdout",
        "on-stderr",
        "dir=/job",
        "\"distribution\": \"rolling\"",
        "from the input file",
    ] {
        assert!(
            log.contains(expected),
            "log is missing {expected:?}:\n{log}"
        );
    }

    let mut outputs = result.outputs();
    outputs.sort();
    assert_eq!(outputs, ["out/results.json", "out/sub/file.txt"]);
    assert_eq!(
        fs::read_to_string(result.work_dir.join("out/results.json")).unwrap(),
        "{\"tests\": []}\n"
    );
    assert_eq!(
        fs::read_to_string(result.work_dir.join("out/sub/file.txt")).unwrap(),
        "deep\n"
    );
}

#[test]
fn failing_job_is_a_build_failure_and_keeps_its_outputs() {
    let Some(runtime) = runtime() else { return };
    let script = "echo building; echo partial > /job/out/partial.txt; exit 3";
    let result = run(&runtime, "failure", &shell_job(&image(), script, 120, &[]));
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
        &shell_job(&image(), "exit 75", 120, &[]),
    );
    assert_eq!(result.outcome(), "infrastructure_failure");
    assert_eq!(result.exit_status, 2);
    assert_eq!(result.outcome.get("exit_code").unwrap().as_i64(), Some(75));
}

#[test]
fn job_past_its_timeout_is_killed() {
    let Some(runtime) = runtime() else { return };
    let script = "echo started; echo before > /job/out/before.txt; sleep 600";
    let started = Instant::now();
    let result = run(&runtime, "timeout", &shell_job(&image(), script, 3, &[]));
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the job should have been killed at its timeout"
    );
    assert_eq!(result.outcome(), "timeout");
    assert_eq!(result.exit_status, 1);
    assert!(result.outcome.get("exit_code").unwrap().is_null());
    assert_eq!(result.outputs(), ["out/before.txt"]);
    assert!(result.log().contains("started"));
}

#[test]
fn missing_image_is_an_infrastructure_failure() {
    let Some(runtime) = runtime() else { return };
    let job = shell_job("localhost:1/ros2ci-no-such-image:1", "true", 120, &[]);
    let result = run(&runtime, "missing_image", &job);
    assert_eq!(result.outcome(), "infrastructure_failure");
    assert_eq!(result.exit_status, 2);
    assert!(result.message().contains("image"), "{}", result.message());
}

#[test]
fn missing_input_is_an_infrastructure_failure() {
    let Some(runtime) = runtime() else { return };
    let job = shell_job(
        &image(),
        "true",
        120,
        &[("x", Path::new("/nonexistent/input"))],
    );
    let result = run(&runtime, "missing_input", &job);
    assert_eq!(result.outcome(), "infrastructure_failure");
    assert_eq!(result.exit_status, 2);
    assert!(result.message().contains("input x"), "{}", result.message());
}

#[test]
fn large_outputs_and_logs_survive_intact() {
    let Some(runtime) = runtime() else { return };
    // 20 MB of pseudo-random data, whose checksum the job prints first,
    // then 20,000 log lines.
    let script = "head -c 20000000 /dev/urandom > /job/out/big.bin; \
                  cksum < /job/out/big.bin; \
                  i=0; while [ $i -lt 20000 ]; do echo line $i; i=$((i+1)); done";
    let result = run(&runtime, "large", &shell_job(&image(), script, 300, &[]));
    assert_eq!(result.outcome(), "success", "{}", result.message());
    let log = result.log();
    let mut lines = log.lines();
    let reported = lines.next().unwrap().to_string();
    assert_eq!(lines.clone().count(), 20000);
    assert_eq!(lines.last(), Some("line 19999"));

    let data = fs::read(result.work_dir.join("out/big.bin")).unwrap();
    assert_eq!(data.len(), 20_000_000);
    assert_eq!(reported, format!("{} {}", posix_cksum(&data), data.len()));
}

/// The POSIX `cksum` algorithm, to compare a file with what the job saw
/// without depending on a hashing library.
fn posix_cksum(data: &[u8]) -> u32 {
    fn step(mut crc: u32, byte: u8) -> u32 {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
        crc
    }
    let mut crc = 0u32;
    for &byte in data {
        crc = step(crc, byte);
    }
    let mut len = data.len();
    while len != 0 {
        crc = step(crc, (len & 0xff) as u8);
        len >>= 8;
    }
    !crc
}
