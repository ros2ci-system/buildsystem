// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The whole system together: the coordinator, real agents, the admin
//! tool, a PostgreSQL server and real containers.
//!
//! These tests need both `CI_POSTGRES_URL` and `CI_CONTAINER_RUNTIME`, and
//! are skipped without them. They run the three programs from the build
//! directory, and `cargo test` does not build all of them, so build the
//! workspace first: `cargo build --workspace`.

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ci_http::server::{serve, Request, Response};
use ci_json::Value;

static NEXT: AtomicU32 = AtomicU32::new(0);

/// What differs between Linux and Windows containers.
struct Platform {
    image: &'static str,
    shell: [&'static str; 2],
    /// Prints its input file, writes two outputs, and succeeds.
    work: &'static str,
    /// Prints a line, then waits ten minutes.
    wait: &'static str,
    /// Prints lines for about six seconds, writes an output, succeeds.
    slow: &'static str,
    /// Prints a line, then fails with exit code 3.
    fail: &'static str,
}

#[cfg(not(windows))]
const PLATFORM: Platform = Platform {
    image: "docker.io/library/alpine:3.20",
    shell: ["sh", "-c"],
    work: "echo working; cat /job/in/input.txt; echo done > /job/out/result.txt; \
           mkdir /job/out/sub; cp /job/in/input.txt /job/out/sub/copy.txt",
    wait: "echo waiting; sleep 600",
    slow: "for i in 1 2 3 4 5 6; do echo step $i; sleep 1; done; echo done > /job/out/result.txt",
    fail: "echo about to fail; exit 3",
};

#[cfg(windows)]
const PLATFORM: Platform = Platform {
    image: "mcr.microsoft.com/windows/nanoserver:ltsc2025",
    shell: ["cmd", "/c"],
    work: "echo working& type C:\\job\\in\\input.txt& echo done> C:\\job\\out\\result.txt& \
           mkdir C:\\job\\out\\sub& copy C:\\job\\in\\input.txt C:\\job\\out\\sub\\copy.txt >NUL",
    wait: "echo waiting& ping -n 600 127.0.0.1 >NUL",
    slow: "(for /L %i in (1,1,6) do @(echo step %i& ping -n 2 127.0.0.1 >NUL))& echo done> C:\\job\\out\\result.txt",
    fail: "echo about to fail& exit 3",
};

struct Services {
    postgres: String,
    runtime: String,
}

fn services() -> Option<Services> {
    let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    match (get("CI_POSTGRES_URL"), get("CI_CONTAINER_RUNTIME")) {
        (Some(postgres), Some(runtime)) => Some(Services { postgres, runtime }),
        _ => {
            eprintln!("skipped: CI_POSTGRES_URL and CI_CONTAINER_RUNTIME are not both set");
            None
        }
    }
}

/// The path of one of the workspace's programs, next to this test.
fn program(name: &str) -> PathBuf {
    let mut dir = std::env::current_exe().unwrap();
    dir.pop(); // the test binary
    if dir.ends_with("deps") {
        dir.pop();
    }
    let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "{} is missing; run `cargo build --workspace` first",
        path.display()
    );
    path
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../net/tests/fixtures")
        .join(name)
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Kills a child process when dropped.
struct Process(Child);

impl Process {
    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

struct System {
    services: Services,
    dir: PathBuf,
    database: String,
    database_url: String,
    worker_port: u16,
    admin_port: u16,
    tls: bool,
    coordinator: Process,
    agents: Vec<Process>,
}

impl System {
    fn start(tls: bool, coordinator_options: &[&str]) -> Option<System> {
        let services = services()?;
        let database = format!(
            "ros2ci_e2e_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        let mut admin = ci_postgres::Connection::connect(
            &ci_postgres::Config::parse(&services.postgres).unwrap(),
        )
        .unwrap();
        admin
            .batch(&format!("DROP DATABASE IF EXISTS {database}"))
            .unwrap();
        admin.batch(&format!("CREATE DATABASE {database}")).unwrap();
        let database_url = format!(
            "{}/{database}",
            services.postgres.rsplit_once('/').unwrap().0
        );
        let dir = std::env::temp_dir().join(&database);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut system = System {
            services,
            dir,
            database,
            database_url,
            worker_port: free_port(),
            admin_port: free_port(),
            tls,
            coordinator: Process(
                Command::new(program("ci-admin"))
                    .arg("--help")
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap(),
            ),
            agents: Vec::new(),
        };
        system.coordinator = system.spawn_coordinator(coordinator_options);
        Some(system)
    }

    fn spawn_coordinator(&self, options: &[&str]) -> Process {
        let mut command = Command::new(program("ci-coordinator"));
        command
            .env("CI_DATABASE_URL", &self.database_url)
            .arg("--data-dir")
            .arg(self.dir.join("data"))
            .arg("--worker-listen")
            .arg(format!("127.0.0.1:{}", self.worker_port))
            .arg("--admin-listen")
            .arg(format!("127.0.0.1:{}", self.admin_port))
            .args([
                "--report-interval-seconds",
                "1",
                "--claim-interval-seconds",
                "1",
            ])
            .args(options)
            .stderr(fs::File::create(self.dir.join("coordinator.log")).unwrap());
        if self.tls {
            command
                .arg("--tls-cert")
                .arg(fixture("server.pem"))
                .arg("--tls-key")
                .arg(fixture("server.key"));
        }
        let process = Process(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect(("127.0.0.1", self.worker_port)).is_err()
            || TcpStream::connect(("127.0.0.1", self.admin_port)).is_err()
        {
            assert!(Instant::now() < deadline, "the coordinator did not start");
            thread::sleep(Duration::from_millis(50));
        }
        process
    }

    fn admin(&self, args: &[&str]) -> Output {
        Command::new(program("ci-admin"))
            .arg("--admin-url")
            .arg(format!("http://127.0.0.1:{}", self.admin_port))
            .arg("--token-file")
            .arg(self.dir.join("data/admin-token"))
            .args(args)
            .output()
            .unwrap()
    }

    fn admin_ok(&self, args: &[&str]) -> String {
        let output = self.admin(args);
        assert!(
            output.status.success(),
            "ci-admin {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Starts an agent in a pool and returns its index.
    fn start_agent(&mut self, pool: &str) -> usize {
        let index = self.agents.len();
        let process = self.spawn_agent(pool, index);
        self.agents.push(process);
        index
    }

    /// Starts the agent with this index again, in the same work
    /// directory, as when a crashed agent is restarted.
    fn restart_agent(&mut self, pool: &str, index: usize) {
        self.agents[index].kill();
        self.agents[index] = self.spawn_agent(pool, index);
    }

    fn spawn_agent(&self, pool: &str, index: usize) -> Process {
        let token = self.admin_ok(&[
            "create-worker-token",
            "--name",
            "test agent",
            "--pool",
            pool,
        ]);
        let token_file = self.dir.join(format!("token-{index}"));
        fs::write(&token_file, token).unwrap();
        let scheme = if self.tls { "https" } else { "http" };
        let mut command = Command::new(program("ci-agent"));
        command
            .arg("work")
            .arg("--coordinator")
            .arg(format!("{scheme}://localhost:{}", self.worker_port))
            .arg("--token-file")
            .arg(&token_file)
            .arg("--work-dir")
            .arg(self.dir.join(format!("agent-{index}")))
            .arg("--runtime")
            .arg(&self.services.runtime)
            .stderr(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.dir.join(format!("agent-{index}.log")))
                    .unwrap(),
            );
        if self.tls {
            command.arg("--ca-file").arg(fixture("ca.pem"));
        }
        Process(command.spawn().unwrap())
    }

    /// Queues a job that runs `script`, and returns its id.
    fn submit(&self, pool: &str, script: &str, timeout: i64, inputs: &[(&str, String)]) -> String {
        let description = Value::object([
            ("image", Value::from(PLATFORM.image)),
            (
                "command",
                Value::strings([PLATFORM.shell[0], PLATFORM.shell[1], script]),
            ),
            (
                "inputs",
                Value::Array(
                    inputs
                        .iter()
                        .map(|(name, url)| {
                            Value::object([
                                ("name", Value::from(*name)),
                                ("url", Value::from(url.as_str())),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("timeout_seconds", Value::Int(timeout)),
        ]);
        let file = self
            .dir
            .join(format!("job-{}.json", NEXT.fetch_add(1, Ordering::SeqCst)));
        fs::write(&file, description.to_json_pretty()).unwrap();
        let (os, arch) = self.platform();
        self.admin_ok(&[
            "submit",
            "--pool",
            pool,
            "--os",
            &os,
            "--arch",
            &arch,
            "--job",
            file.to_str().unwrap(),
        ])
        .trim()
        .to_string()
    }

    /// The operating system and architecture the agents register with,
    /// read back from the coordinator's log.
    fn platform(&self) -> (String, String) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let log = fs::read_to_string(self.dir.join("coordinator.log")).unwrap_or_default();
            if let Some(line) = log.lines().find(|l| l.contains("registered: pool")) {
                let platform = line.rsplit(' ').next().unwrap();
                let (os, arch) = platform.split_once('/').unwrap();
                return (os.to_string(), arch.to_string());
            }
            assert!(Instant::now() < deadline, "no agent registered:\n{log}");
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn status(&self, job: &str) -> Value {
        ci_json::parse(&self.admin_ok(&["status", job])).unwrap()
    }

    fn wait_for(&self, job: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let status = self.status(job);
            if done(&status) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "job {job} never reached: {what}\nstatus: {}\ncoordinator log:\n{}\nagent logs:\n{}",
                status.to_json_pretty(),
                fs::read_to_string(self.dir.join("coordinator.log")).unwrap_or_default(),
                (0..self.agents.len())
                    .map(|i| fs::read_to_string(self.dir.join(format!("agent-{i}.log"))).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n---\n"),
            );
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn wait_finished(&self, job: &str) -> Value {
        self.wait_for(job, "finished", |s| str_of(s, "state") == "finished")
    }

    fn wait_for_log(&self, job: &str, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(180);
        while !self.admin_ok(&["log", job]).contains(text) {
            assert!(
                Instant::now() < deadline,
                "job {job}'s log never showed {text:?}"
            );
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn artifact(&self, job: &str, name: &str) -> Vec<u8> {
        let file = self
            .dir
            .join(format!("fetched-{}", NEXT.fetch_add(1, Ordering::SeqCst)));
        self.admin_ok(&["artifact", job, name, "--output", file.to_str().unwrap()]);
        fs::read(file).unwrap()
    }
}

impl Drop for System {
    fn drop(&mut self) {
        for agent in &mut self.agents {
            agent.kill();
        }
        self.coordinator.kill();
        if thread::panicking() {
            // Leave the logs where a person can read them.
            eprintln!("logs kept in {}", self.dir.display());
        } else {
            let _ = fs::remove_dir_all(&self.dir);
        }
        if let Ok(mut admin) = ci_postgres::Connection::connect(
            &ci_postgres::Config::parse(&self.services.postgres).unwrap(),
        ) {
            let _ = admin.batch(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.database
            ));
        }
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn artifact_names(status: &Value) -> Vec<&str> {
    status
        .get("artifacts")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|a| str_of(a, "path"))
        .collect()
}

/// Serves one fixed body at every path, and returns the server's URL.
fn serve_input(body: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        serve(listener, 2, Ok, move |request: &mut Request<'_>| {
            if request.path == "/missing" {
                Response::empty(404)
            } else {
                Response::new(200, "application/octet-stream", body)
            }
        })
    });
    format!("http://127.0.0.1:{port}")
}

fn assert_no_job_directories(system: &System, agent: usize) {
    let dir = system.dir.join(format!("agent-{agent}"));
    let left: Vec<_> = fs::read_dir(&dir)
        .map(|d| {
            d.flatten()
                .filter(|e| e.file_name() != "agent-id")
                .collect()
        })
        .unwrap_or_default();
    assert!(
        left.is_empty(),
        "the agent left job directories behind in {}",
        dir.display()
    );
}

fn has(path: &Path) -> bool {
    path.exists()
}

#[test]
fn a_job_runs_end_to_end_over_tls() {
    let Some(mut system) = System::start(true, &[]) else {
        return;
    };
    let agent = system.start_agent("release");
    let input = serve_input(b"contents of the input\n");

    let job = system.submit(
        "release",
        PLATFORM.work,
        120,
        &[("input.txt", format!("{input}/file"))],
    );
    let done = system.wait_finished(&job);
    assert_eq!(
        str_of(&done, "outcome"),
        "success",
        "{}",
        done.to_json_pretty()
    );
    assert_eq!(done.get("exit_code").unwrap().as_i64(), Some(0));
    assert_eq!(done.get("attempt").unwrap().as_i64(), Some(1));
    assert!(str_of(&done, "image_id").starts_with("sha256:"));

    let log = system.admin_ok(&["log", &job]);
    assert!(log.contains("working"), "{log}");
    assert!(log.contains("contents of the input"), "{log}");

    let mut names = artifact_names(&done);
    names.sort();
    assert_eq!(names, ["result.txt", "sub/copy.txt"]);
    assert_eq!(
        String::from_utf8(system.artifact(&job, "result.txt"))
            .unwrap()
            .trim(),
        "done"
    );
    assert_eq!(
        system.artifact(&job, "sub/copy.txt"),
        b"contents of the input\n"
    );

    assert_no_job_directories(&system, agent);
    assert!(has(&system
        .dir
        .join("data/jobs")
        .join(&job)
        .join("1/log.txt")));
}

#[test]
fn submit_and_wait_follows_the_log_and_reports_failure() {
    let Some(mut system) = System::start(false, &[]) else {
        return;
    };
    system.start_agent("release");
    let (os, arch) = system.platform();
    let write_job = |name: &str, script: &str| {
        let description = Value::object([
            ("image", Value::from(PLATFORM.image)),
            (
                "command",
                Value::strings([PLATFORM.shell[0], PLATFORM.shell[1], script]),
            ),
            ("timeout_seconds", Value::Int(120)),
        ]);
        let file = system.dir.join(name);
        fs::write(&file, description.to_json_pretty()).unwrap();
        file
    };

    let passing = write_job("passing.json", "echo all good");
    let output = system.admin(&[
        "submit",
        "--pool",
        "release",
        "--os",
        &os,
        "--arch",
        &arch,
        "--job",
        passing.to_str().unwrap(),
        "--wait",
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("all good"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("success"));

    let failing = write_job("failing.json", PLATFORM.fail);
    let output = system.admin(&[
        "submit",
        "--pool",
        "release",
        "--os",
        &os,
        "--arch",
        &arch,
        "--job",
        failing.to_str().unwrap(),
        "--wait",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("about to fail"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("build_failure"));
}

#[test]
fn a_running_job_can_be_cancelled() {
    let Some(mut system) = System::start(false, &[]) else {
        return;
    };
    let agent = system.start_agent("release");
    let job = system.submit("release", PLATFORM.wait, 900, &[]);
    system.wait_for_log(&job, "waiting");

    let started = Instant::now();
    system.admin_ok(&["cancel", &job]);
    let done = system.wait_finished(&job);
    assert_eq!(str_of(&done, "outcome"), "cancelled");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the cancel should take effect within a few report intervals"
    );
    assert_no_job_directories(&system, agent);

    // The agent is free again and takes the next job.
    let next = system.submit("release", "echo next", 120, &[]);
    assert_eq!(str_of(&system.wait_finished(&next), "outcome"), "success");
}

#[test]
fn a_job_whose_agent_dies_is_run_again_by_another() {
    let Some(mut system) = System::start(
        false,
        &["--lease-seconds", "4", "--retry-backoff-seconds", "1"],
    ) else {
        return;
    };
    let first = system.start_agent("release");
    let job = system.submit("release", PLATFORM.slow, 300, &[]);
    system.wait_for_log(&job, "step 2");

    // The agent is killed outright, with no chance to say anything.
    system.agents[first].kill();
    let requeued = system.wait_for(&job, "a second attempt", |s| {
        s.get("infrastructure_failures").and_then(Value::as_i64) == Some(1)
    });
    assert_ne!(str_of(&requeued, "state"), "finished");

    system.start_agent("release");
    let done = system.wait_finished(&job);
    assert_eq!(
        str_of(&done, "outcome"),
        "success",
        "{}",
        done.to_json_pretty()
    );
    assert_eq!(done.get("attempt").unwrap().as_i64(), Some(2));
    // The log and artifacts are the second attempt's, complete.
    let log = system.admin_ok(&["log", &job]);
    assert_eq!(log.matches("step 1").count(), 1, "{log}");
    assert!(log.contains("step 6"), "{log}");
    assert_eq!(artifact_names(&done), ["result.txt"]);

    // When the dead agent is started again, it removes the container its
    // first run left behind.
    system.restart_agent("release", first);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let log =
            fs::read_to_string(system.dir.join(format!("agent-{first}.log"))).unwrap_or_default();
        if log.contains("removed a container left by an earlier run") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the restarted agent did not clean up:\n{log}"
        );
        thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn a_coordinator_restart_does_not_disturb_a_running_job() {
    let Some(mut system) = System::start(false, &["--lease-seconds", "4"]) else {
        return;
    };
    system.start_agent("release");
    let job = system.submit("release", PLATFORM.slow, 300, &[]);
    system.wait_for_log(&job, "step 1");

    // Down for longer than the lease, while the job keeps running.
    system.coordinator.kill();
    thread::sleep(Duration::from_secs(6));
    system.coordinator = system.spawn_coordinator(&["--lease-seconds", "4"]);

    let done = system.wait_finished(&job);
    assert_eq!(
        str_of(&done, "outcome"),
        "success",
        "{}",
        done.to_json_pretty()
    );
    assert_eq!(
        done.get("attempt").unwrap().as_i64(),
        Some(1),
        "the job must not have been restarted"
    );
    let log = system.admin_ok(&["log", &job]);
    for step in 1..=6 {
        assert_eq!(log.matches(&format!("step {step}")).count(), 1, "{log}");
    }
    assert_eq!(artifact_names(&done), ["result.txt"]);
}

#[test]
fn problems_outside_the_job_are_infrastructure_failures() {
    let Some(mut system) = System::start(
        false,
        &[
            "--max-infrastructure-failures",
            "2",
            "--retry-backoff-seconds",
            "1",
        ],
    ) else {
        return;
    };
    system.start_agent("release");
    let input = serve_input(b"unused");

    // An input that cannot be downloaded: tried twice, then given up on.
    let job = system.submit(
        "release",
        "echo never runs",
        120,
        &[("x", format!("{input}/missing"))],
    );
    let done = system.wait_finished(&job);
    assert_eq!(str_of(&done, "outcome"), "infrastructure_failure");
    assert_eq!(
        done.get("infrastructure_failures").unwrap().as_i64(),
        Some(2)
    );
    assert!(
        str_of(&done, "message").contains("input x"),
        "{}",
        str_of(&done, "message")
    );
    assert!(!system.admin_ok(&["log", &job]).contains("never runs"));

    // A job that fails by itself is a build failure, and is not retried.
    let failing = system.submit("release", "exit 3", 120, &[]);
    let done = system.wait_finished(&failing);
    assert_eq!(str_of(&done, "outcome"), "build_failure");
    assert_eq!(done.get("attempt").unwrap().as_i64(), Some(1));
    assert_eq!(done.get("exit_code").unwrap().as_i64(), Some(3));

    // So is one that runs past its timeout.
    let slow = system.submit("release", PLATFORM.wait, 3, &[]);
    let done = system.wait_finished(&slow);
    assert_eq!(str_of(&done, "outcome"), "timeout");
    assert_eq!(done.get("attempt").unwrap().as_i64(), Some(1));
}

#[test]
fn pools_are_kept_apart() {
    let Some(mut system) = System::start(false, &[]) else {
        return;
    };
    system.start_agent("pull-request");
    let release_job = system.submit("release", "echo release work", 120, &[]);
    let pr_job = system.submit("pull-request", "echo pr work", 120, &[]);

    assert_eq!(str_of(&system.wait_finished(&pr_job), "outcome"), "success");
    // The only agent serves pull requests, so the release job waits.
    thread::sleep(Duration::from_secs(3));
    assert_eq!(str_of(&system.status(&release_job), "state"), "queued");

    system.start_agent("release");
    assert_eq!(
        str_of(&system.wait_finished(&release_job), "outcome"),
        "success"
    );
}

#[test]
fn a_restarted_agent_removes_the_container_it_left_behind() {
    let Some(mut system) = System::start(
        false,
        &["--lease-seconds", "4", "--retry-backoff-seconds", "1"],
    ) else {
        return;
    };
    let agent = system.start_agent("release");
    let job = system.submit("release", PLATFORM.wait, 900, &[]);
    system.wait_for_log(&job, "waiting");

    // Killed outright: the container it started keeps running, with ten
    // minutes of waiting still to do.
    system.agents[agent].kill();
    system.restart_agent("release", agent);

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let log =
            fs::read_to_string(system.dir.join(format!("agent-{agent}.log"))).unwrap_or_default();
        if log.contains("removed a container left by an earlier run") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the restarted agent did not clean up:\n{log}"
        );
        thread::sleep(Duration::from_millis(250));
    }

    // The job comes round again as a second attempt; cancel it so the
    // test does not wait ten minutes.
    system.wait_for(&job, "a second attempt running", |s| {
        s.get("attempt").and_then(Value::as_i64) == Some(2) && str_of(s, "state") == "running"
    });
    system.admin_ok(&["cancel", &job]);
    assert_eq!(str_of(&system.wait_finished(&job), "outcome"), "cancelled");
    assert_no_job_directories(&system, agent);
}

#[test]
fn the_admin_tool_needs_a_token() {
    let Some(mut system) = System::start(false, &[]) else {
        return;
    };
    let url = format!("http://127.0.0.1:{}", system.admin_port);
    let run = |token_file: Option<&Path>, args: &[&str]| {
        let mut command = Command::new(program("ci-admin"));
        command
            .env_remove("CI_ADMIN_TOKEN_FILE")
            .arg("--admin-url")
            .arg(&url);
        if let Some(file) = token_file {
            command.arg("--token-file").arg(file);
        }
        command.args(args).output().unwrap()
    };

    // No token file at all.
    let output = run(None, &["status", "1"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("admin token"));

    // A file holding something that is not a token.
    let wrong = system.dir.join("wrong-token");
    fs::write(&wrong, "not-a-token\n").unwrap();
    let output = run(
        Some(&wrong),
        &["create-worker-token", "--name", "x", "--pool", "release"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("401"));
    assert!(output.stdout.is_empty(), "no token may be printed");

    // A second administrator gets a token of their own, which works, and
    // which the first can take away again.
    let second = system.dir.join("second-token");
    fs::write(
        &second,
        system.admin_ok(&["create-admin-token", "--name", "second administrator"]),
    )
    .unwrap();
    assert!(run(
        Some(&second),
        &["create-worker-token", "--name", "x", "--pool", "release"]
    )
    .status
    .success());
    system.admin_ok(&["revoke-admin-token", "2"]);
    assert!(!run(Some(&second), &["status", "1"]).status.success());

    // The coordinator's log says which administrator did what.
    system.start_agent("release");
    let job = system.submit("release", "echo audited", 120, &[]);
    system.wait_finished(&job);
    let log = fs::read_to_string(system.dir.join("coordinator.log")).unwrap();
    assert!(
        log.contains("first admin token, created at start-up"),
        "{log}"
    );
    assert!(log.contains(&format!("submitted job {job}")), "{log}");
    assert!(
        log.contains("admin \"second administrator\": created worker token"),
        "{log}"
    );
}
