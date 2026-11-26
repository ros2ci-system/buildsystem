// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The scale test's tool. It makes up a ROS distribution of any size and
//! stands in for everything outside the coordinator: the rosdistro's git
//! server, and workers whose jobs finish at once. What is left to
//! measure is the coordinator, the reconciler and the database.

mod answers;
mod distribution;
mod worker;

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use ci_net::{Connector, Url};
use distribution::Distribution;
use worker::{Counters, Group, Shared};

const USAGE: &str = "\
Usage: ci-scale <command> [options]

  generate --packages <n> [--seed <n>] --output <file>
      Writes a made-up distribution with the shape of a real one.

  describe <file>
      Prints the shape of a distribution: its size, how deep its
      dependencies go, and how much stands on its lowest package.

  bump <file> --package <name> [--rebuilt <file>]
      Gives the repository that holds the package a new version, as a
      release would. With --rebuilt, writes the names of the packages
      that must be built again because of it, one a line.

  run --distribution <file> --coordinator <url> --git-listen <addr>
      --workers <os>:<arch>:<count>:<token file> ... [--job-millis <n>]
      Stands in for the rosdistro's git server, which answers at
      http://<addr>/rosdistro.git with one branch, main, and for workers.
      A worker's pool is that of its token. Jobs finish at once, or after
      --job-millis. Runs until stopped.

The package build jobs must carry a parameter named scale that says what
to pretend to build: an object with format (deb or rpm), suffix (what
follows the rebuild counter in a version) and prefix (what goes before a
package's name).
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(&args[0], &args[1..]) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("ci-scale: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The options of a command: those that take a value, by name, and the
/// arguments that are not options.
struct Options {
    named: Vec<(String, String)>,
    plain: Vec<String>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut options = Options {
            named: Vec::new(),
            plain: Vec::new(),
        };
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.strip_prefix("--") {
                Some(name) => {
                    let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
                    options.named.push((name.to_string(), value.clone()));
                }
                None => options.plain.push(arg.clone()),
            }
        }
        Ok(options)
    }

    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.named
            .iter()
            .filter(move |(n, _)| n == name)
            .map(|(_, value)| value.as_str())
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.named
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.as_str())
    }

    fn required(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }

    fn number(&self, name: &str, default: u64) -> Result<u64, String> {
        match self.get(name) {
            None => Ok(default),
            Some(text) => text
                .parse()
                .map_err(|_| format!("--{name} needs a whole number")),
        }
    }
}

fn read_distribution(path: &str) -> Result<Distribution, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    Distribution::parse(&text).map_err(|e| format!("{path}: {e}"))
}

/// Writes under another name first, so that nobody reads half a file.
fn write_whole(path: &str, contents: &str) -> Result<(), String> {
    let temporary = format!("{path}.new");
    fs::write(&temporary, contents)
        .and_then(|()| fs::rename(&temporary, path))
        .map_err(|e| format!("cannot write {path}: {e}"))
}

fn run(command: &str, args: &[String]) -> Result<(), String> {
    let options = Options::parse(args)?;
    match command {
        "generate" => {
            let packages = options.number("packages", 0)? as usize;
            if packages == 0 {
                return Err("--packages is required".to_string());
            }
            let distribution = distribution::generate(packages, options.number("seed", 1)?);
            write_whole(options.required("output")?, &distribution.to_json())?;
            print!("{}", distribution.describe());
            Ok(())
        }
        "describe" => {
            let path = options.plain.first().ok_or("which file?")?;
            let distribution = read_distribution(path)?;
            println!(
                "{}commit {}",
                distribution.describe(),
                distribution.commit()
            );
            Ok(())
        }
        "bump" => {
            let path = options.plain.first().ok_or("which file?")?;
            let package = options.required("package")?;
            let mut distribution = read_distribution(path)?;
            let (repository, version) = distribution.bump(package)?;
            let rebuilt = distribution.rebuilt_with(package, false);
            if let Some(list) = options.get("rebuilt") {
                let names: Vec<&str> = rebuilt.iter().map(String::as_str).collect();
                write_whole(list, &(names.join("\n") + "\n"))?;
            }
            write_whole(path, &distribution.to_json())?;
            println!(
                "{repository} is now {version}; {} packages must be built again; commit {}",
                rebuilt.len(),
                distribution.commit()
            );
            Ok(())
        }
        "run" => stand_in(&options),
        other => Err(format!("unknown command {other:?}; see --help")),
    }
}

fn stand_in(options: &Options) -> Result<(), String> {
    let path = options.required("distribution")?;
    let coordinator = Url::parse(options.required("coordinator")?).map_err(|e| e.to_string())?;
    let mut groups = Vec::new();
    for text in options.all("workers") {
        let parts: Vec<&str> = text.splitn(4, ':').collect();
        let [os, arch, count, token_file] = parts.as_slice() else {
            return Err(format!(
                "--workers {text}: expected os:arch:count:token-file"
            ));
        };
        groups.push(Group {
            os: os.to_string(),
            arch: arch.to_string(),
            count: count
                .parse()
                .map_err(|_| format!("--workers {text}: the count is not a number"))?,
            token: fs::read_to_string(token_file)
                .map_err(|e| format!("cannot read {token_file}: {e}"))?
                .trim()
                .to_string(),
        });
    }
    if groups.is_empty() {
        return Err("--workers is required".to_string());
    }
    let git_listen = options.required("git-listen")?;
    let listener =
        TcpListener::bind(git_listen).map_err(|e| format!("cannot listen on {git_listen}: {e}"))?;
    let distribution = read_distribution(path)?;
    eprintln!(
        "the distribution is at commit {}; its git server is http://{git_listen}/rosdistro.git",
        distribution.commit()
    );
    let shared = Arc::new(Shared {
        coordinator,
        connector: Connector::with_public_roots(None).map_err(|e| e.to_string())?,
        distribution_path: PathBuf::from(path),
        distribution: RwLock::new(Arc::new(distribution)),
        job_time: Duration::from_millis(options.number("job-millis", 0)?),
        counters: Counters::default(),
    });
    shared.follow_distribution();
    shared.serve_git_refs(listener);
    for group in groups {
        for _ in 0..group.count {
            let shared = Arc::clone(&shared);
            let group = group.clone();
            thread::spawn(move || {
                if let Err(message) = shared.work(&group) {
                    eprintln!("a {}/{} worker stopped: {message}", group.os, group.arch);
                }
            });
        }
    }
    // A line whenever something was done since the last one.
    let started = Instant::now();
    let mut last = (0, 0, 0, 0, 0, 0);
    loop {
        thread::sleep(Duration::from_secs(10));
        let counters = &shared.counters;
        let now = (
            counters.checks.load(Ordering::Relaxed),
            counters.archives.load(Ordering::Relaxed),
            counters.builds.load(Ordering::Relaxed),
            counters.refused.load(Ordering::Relaxed),
            counters.retried.load(Ordering::Relaxed),
            counters.errors.load(Ordering::Relaxed),
        );
        if now != last {
            eprintln!(
                "{:>6}s: {} release checks, {} source archives, {} builds, {} jobs it could not answer, {} calls that did not get through, {} that got an error",
                started.elapsed().as_secs(),
                now.0,
                now.1,
                now.2,
                now.3,
                now.4,
                now.5
            );
            last = now;
        }
    }
}
