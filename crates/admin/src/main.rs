// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The administrator's command-line tool. It talks to the coordinator's
//! admin API, which listens on a private address.

use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use ci_http::{Body, Request};
use ci_json::Value;
use ci_net::{Connector, Url};
use ci_openpgp::SigningKey;

mod release;

const USAGE: &str = "\
Usage: ci-admin [--admin-url <url>] [--token-file <file>] <command>

Commands:
  create-worker-token --name <text> --pool <pool>
      Issues an enrolment token for a worker and prints it. The token is
      shown once and cannot be recovered.
  revoke-worker-token <token-id>
      Revokes a worker's enrolment token. Workers that registered with it
      are refused from then on.
  list-workers
      Lists the registered workers: id, pool, platform, when each last
      called, the job it is running, and whether it is draining.
  drain-worker <worker-id>
      Stops a worker cleanly. It finishes the job it is running, takes no
      more, and its agent exits with status 0. An agent that is started
      again registers as a new worker and takes jobs as usual.
  create-admin-token --name <text>
      Issues another admin token and prints it. Give each person or
      service its own, so that the coordinator's log says who did what.
  revoke-admin-token <token-id>
      Revokes an admin token.
  store-file <name> <file>
      Stores a file on the coordinator under a name, replacing any earlier
      one. A job reads it by naming it as stored:<name>, and only the
      worker running such a job can download it.
  store-context <name> <directory>
      Packs a directory into a build context and stores it under a name.
      The same directory contents always make the same context, so an
      unchanged image definition is not rebuilt.
  list-files
      Lists the stored files.
  delete-file <name>
      Deletes a stored file.
  submit --pool <pool> --os <os> --arch <arch> --job <file> [--wait]
      Queues a job from a job description file and prints its id. With
      --wait, follows the job's log until it finishes; the exit status is
      then 0 for success, 1 for a failed job, 2 for an infrastructure
      failure.
  status <job-id>
      Prints a job's state as JSON.
  log <job-id>
      Prints the log of a job's latest attempt.
  artifact <job-id> <path> --output <file>
      Downloads one artifact of a job.
  cancel <job-id>
      Cancels a job.
  release --rosdistro-url <url> --commit <commit> --check-job <file>
          --archive-job <file> [--rosdistro <name>] [--check-pool <pool>]
          [--archive-pool <pool>] [--os <os>] [--arch <arch>]
      Takes the releases of a rosdistro commit into the system. The URL is
      where the rosdistro's files are, without the commit. It runs the
      release check, which looks at every release and builds nothing, and
      then archives the source of each release the archive does not hold.
      A tag that no longer names the commit that was archived is reported.
      The two files describe the jobs; this command adds their parameters.
      Defaults: rosdistro rolling, pools pull-request and release,
      linux/amd64. Exit status 1 when the check found a problem or a
      release could not be archived; nothing is archived after a problem.
  archived [--rosdistro <name>]
      Lists the releases in the source archive, as JSON.
  generate-signing-key --user-id <text> --output <file>
      Makes the key that signs repository indexes and writes it to a new
      file that only you can read. It talks to no coordinator and needs
      no token. The coordinator reads the file as --signing-key. The
      user id says whose key it is, conventionally: Name <address>
  create-repository --name <name> --format apt|rpm --version-suffix <text>
                    --architectures <arch>[,<arch>] --source-arch <arch>
                    [--suite <suite>]
      Creates an empty, signed package repository. The version suffix
      follows the rebuild counter in package versions: the release code
      name for apt (resolute), the dist tag for RPM (.el10). Source
      packages are taken from the builds of the source architecture. An
      apt repository needs its suite.
  repository <name>
      Prints a repository's settings and what is published in it, as JSON.
  repository-key <name>
      Prints the public key that a repository's index is signed with.
  next-revision --repository <name> --package <package> --version <version>
      Hands out the rebuild counter for the next build of a version of a
      package and prints it: 1 the first time, one more each time after.
      Give it to the build as its revision.
  publish --repository <name> --job <job-id> --package <package>
          --version <version> --revision <n> --name <name> [--name <name>]
      Publishes the packages a successful job built. Each --name is a
      binary or source package name the build may publish; anything else
      the job built, or a version other than the one stated, is refused.

  --admin-url <url>    The coordinator's admin API. Defaults to the
                       CI_ADMIN_URL environment variable, then to
                       http://127.0.0.1:8444.
  --token-file <file>  A file holding an admin token. Defaults to the
                       CI_ADMIN_TOKEN_FILE environment variable. The
                       coordinator writes the first token to the file
                       admin-token in its data directory.
";

struct Admin {
    base: Url,
    /// The `Authorization` header value.
    authorization: String,
}

impl Admin {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, ci_http::Response<ci_net::Stream>), String> {
        let url = self.base.with_target(path);
        let host = url.host_header();
        let mut request =
            Request::new(method, &url.target, &host).header("Authorization", &self.authorization);
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(Body::Bytes(body.as_bytes()));
        }
        let response = Connector::plain_only().send(&url, request).map_err(|e| {
            format!(
                "cannot reach the coordinator's admin API at {}: {e}",
                self.base.host_header()
            )
        })?;
        Ok((response.status, response))
    }

    /// Makes a call that answers with JSON, and fails on an error status.
    fn json(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let text = body.map(Value::to_json);
        let (status, mut response) = self.call(method, path, text.as_deref())?;
        let bytes = response.body_bytes(64 << 20).map_err(|e| e.to_string())?;
        let value = ci_json::parse_bytes(&bytes)
            .map_err(|e| format!("the coordinator's reply was not JSON: {e}"))?;
        if !(200..300).contains(&status) {
            let reason = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            return Err(format!("the coordinator answered {status}: {reason}"));
        }
        Ok(value)
    }

    /// Stores a file on the coordinator under a name.
    fn store(&self, name: &str, path: &std::path::Path) -> Result<Value, String> {
        let mut file =
            fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        let url = self.base.with_target(&format!(
            "/admin/v1/files/{}",
            ci_http::percent_encode(name)
        ));
        let host = url.host_header();
        let request = Request::new("PUT", &url.target, &host)
            .header("Authorization", &self.authorization)
            .header("Content-Type", "application/octet-stream")
            .body(Body::Sized(&mut file, size));
        let mut response = Connector::plain_only().send(&url, request).map_err(|e| {
            format!(
                "cannot reach the coordinator's admin API at {}: {e}",
                self.base.host_header()
            )
        })?;
        let bytes = response.body_bytes(1 << 20).map_err(|e| e.to_string())?;
        let value = ci_json::parse_bytes(&bytes)
            .map_err(|e| format!("the coordinator's reply was not JSON: {e}"))?;
        if response.status != 200 {
            let reason = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            return Err(format!(
                "the coordinator answered {}: {reason}",
                response.status
            ));
        }
        Ok(value)
    }

    /// Copies everything of a job's log after `from` to standard output,
    /// and returns the new length.
    fn print_log(&self, job_id: i64, from: u64) -> Result<u64, String> {
        let (status, mut response) =
            self.call("GET", &format!("/admin/v1/jobs/{job_id}/log"), None)?;
        if status != 200 {
            return Err(format!("the coordinator answered {status} for the log"));
        }
        // The log is served whole. Skip what was already shown.
        let skipped = io::copy(&mut (&mut response).take(from), &mut io::sink())
            .map_err(|e| e.to_string())?;
        let mut out = io::stdout().lock();
        let copied = io::copy(&mut response, &mut out).map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        Ok(skipped + copied)
    }
}

/// Named options as (name, value) pairs, then positional values.
type Parsed = (Vec<(String, String)>, Vec<String>);

/// Splits arguments into named options and positional values. Every
/// option in `flags` takes no value; every other option takes one.
fn parse_args(args: &[String], flags: &[&str]) -> Result<Parsed, String> {
    let mut options = Vec::new();
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(name) = arg.strip_prefix("--") {
            if flags.contains(&name) {
                options.push((name.to_string(), String::new()));
            } else {
                let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
                options.push((name.to_string(), value.clone()));
            }
        } else {
            positional.push(arg.clone());
        }
    }
    Ok((options, positional))
}

fn option<'a>(options: &'a [(String, String)], name: &str) -> Result<&'a str, String> {
    options
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
        .ok_or_else(|| format!("--{name} is required"))
}

fn only_options(options: &[(String, String)], allowed: &[&str]) -> Result<(), String> {
    match options.iter().find(|(n, _)| !allowed.contains(&n.as_str())) {
        Some((name, _)) => Err(format!("unknown option --{name}")),
        None => Ok(()),
    }
}

fn job_id(positional: &[String]) -> Result<i64, String> {
    positional
        .first()
        .and_then(|text| text.parse().ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| "a job id is required".to_string())
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut admin_url =
        std::env::var("CI_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:8444".to_string());
    let mut token_file = std::env::var("CI_ADMIN_TOKEN_FILE").ok();
    while let Some(name @ ("--admin-url" | "--token-file")) = args.first().map(String::as_str) {
        if args.len() < 2 {
            eprintln!("ci-admin: {name} needs a value");
            return ExitCode::from(64);
        }
        if name == "--admin-url" {
            admin_url = args[1].clone();
        } else {
            token_file = Some(args[1].clone());
        }
        args.drain(..2);
    }
    let Some(command) = args.first().cloned() else {
        eprint!("{USAGE}");
        return ExitCode::from(64);
    };
    if matches!(command.as_str(), "--help" | "-h" | "help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if command == "generate-signing-key" {
        return match generate_signing_key(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("ci-admin: {message}");
                ExitCode::from(3)
            }
        };
    }
    let result = Url::parse(&admin_url)
        .map_err(|e| e.to_string())
        .and_then(|base| {
            if base.https {
                return Err(
                    "the admin API is plain HTTP on a private address; use an http:// URL"
                        .to_string(),
                );
            }
            let token_file = token_file
                .ok_or("an admin token is needed: use --token-file or set CI_ADMIN_TOKEN_FILE")?;
            let token = fs::read_to_string(&token_file)
                .map_err(|e| format!("cannot read {token_file}: {e}"))?
                .trim()
                .to_string();
            if token.is_empty() || token.bytes().any(|b| b.is_ascii_control()) {
                return Err(format!("{token_file} does not hold a token"));
            }
            let admin = Admin {
                base,
                authorization: format!("Bearer {token}"),
            };
            run(&admin, &command, &args[1..])
        });
    match result {
        Ok(code) => code,
        Err(message) => {
            eprintln!("ci-admin: {message}");
            ExitCode::from(3)
        }
    }
}

fn run(admin: &Admin, command: &str, args: &[String]) -> Result<ExitCode, String> {
    match command {
        "create-worker-token" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &["name", "pool"])?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {:?}", positional[0]));
            }
            let reply = admin.json(
                "POST",
                "/admin/v1/worker-tokens",
                Some(&Value::object([
                    ("name", Value::from(option(&options, "name")?)),
                    ("pool", Value::from(option(&options, "pool")?)),
                ])),
            )?;
            println!(
                "{}",
                reply
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or("the reply has no token")?
            );
            Ok(ExitCode::SUCCESS)
        }
        "create-admin-token" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &["name"])?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {:?}", positional[0]));
            }
            let reply = admin.json(
                "POST",
                "/admin/v1/admin-tokens",
                Some(&Value::object([(
                    "name",
                    Value::from(option(&options, "name")?),
                )])),
            )?;
            // The id, for revoking the token later, goes to standard error
            // so that standard output holds only the token.
            eprintln!(
                "token id {}",
                reply.get("token_id").and_then(Value::as_i64).unwrap_or(0)
            );
            println!(
                "{}",
                reply
                    .get("token")
                    .and_then(Value::as_str)
                    .ok_or("the reply has no token")?
            );
            Ok(ExitCode::SUCCESS)
        }
        "revoke-admin-token" | "revoke-worker-token" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &[])?;
            let id = positional
                .first()
                .and_then(|text| text.parse::<i64>().ok())
                .filter(|id| *id > 0)
                .ok_or("a token id is required")?;
            let kind = if command == "revoke-admin-token" {
                "admin-tokens"
            } else {
                "worker-tokens"
            };
            admin.json("POST", &format!("/admin/v1/{kind}/{id}/revoke"), None)?;
            eprintln!("token {id} revoked");
            Ok(ExitCode::SUCCESS)
        }
        "store-file" | "store-context" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &[])?;
            let [name, path] = positional.as_slice() else {
                return Err(format!("{command} takes a name and a path"));
            };
            let reply = if command == "store-file" {
                admin.store(name, std::path::Path::new(path))?
            } else {
                // Packed to a temporary file beside nothing of value, and
                // removed whether or not the upload works.
                let packed = std::env::temp_dir()
                    .join(format!("ci-admin-context-{}.tar", std::process::id()));
                let result = pack_context(std::path::Path::new(path), &packed)
                    .and_then(|()| admin.store(name, &packed));
                let _ = fs::remove_file(&packed);
                result?
            };
            // The hash identifies the contents; for a build context it
            // decides whether an image has to be rebuilt.
            println!(
                "{} {}",
                reply.get("sha256").and_then(Value::as_str).unwrap_or("?"),
                reply.get("name").and_then(Value::as_str).unwrap_or(name)
            );
            Ok(ExitCode::SUCCESS)
        }
        "list-workers" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &[])?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {:?}", positional[0]));
            }
            let reply = admin.json("GET", "/admin/v1/workers", None)?;
            println!(
                "{:>6}  {:<16} {:<16} {:<20}  {:>6}  STATE  TOKEN",
                "WORKER", "POOL", "PLATFORM", "LAST SEEN", "JOB"
            );
            for worker in reply
                .get("workers")
                .and_then(Value::as_array)
                .unwrap_or(&[])
            {
                let text = |key: &str| worker.get(key).and_then(Value::as_str).unwrap_or("?");
                let draining = worker.get("draining") == Some(&Value::Bool(true));
                println!(
                    "{:>6}  {:<16} {:<16} {:<20}  {:>6}  {:<8} {} ({})",
                    worker.get("worker_id").and_then(Value::as_i64).unwrap_or(0),
                    text("pool"),
                    format!("{}/{}", text("os"), text("arch")),
                    text("last_seen_at"),
                    worker
                        .get("job_id")
                        .and_then(Value::as_i64)
                        .map_or("-".to_string(), |id| id.to_string()),
                    if draining { "draining" } else { "active" },
                    worker.get("token_id").and_then(Value::as_i64).unwrap_or(0),
                    text("token_name"),
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        "drain-worker" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &[])?;
            let id = positional
                .first()
                .and_then(|text| text.parse::<i64>().ok())
                .filter(|id| *id > 0)
                .ok_or("a worker id is required")?;
            admin.json("POST", &format!("/admin/v1/workers/{id}/drain"), None)?;
            eprintln!("worker {id} is draining");
            Ok(ExitCode::SUCCESS)
        }
        "list-files" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &[])?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {:?}", positional[0]));
            }
            let reply = admin.json("GET", "/admin/v1/files", None)?;
            for file in reply.get("files").and_then(Value::as_array).unwrap_or(&[]) {
                println!(
                    "{} {:>12} {}",
                    file.get("sha256").and_then(Value::as_str).unwrap_or("?"),
                    file.get("size").and_then(Value::as_i64).unwrap_or(0),
                    file.get("name").and_then(Value::as_str).unwrap_or("?")
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        "delete-file" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &[])?;
            let [name] = positional.as_slice() else {
                return Err("delete-file takes a name".to_string());
            };
            admin.json(
                "DELETE",
                &format!("/admin/v1/files/{}", ci_http::percent_encode(name)),
                None,
            )?;
            eprintln!("{name} deleted");
            Ok(ExitCode::SUCCESS)
        }
        "submit" => {
            let (options, positional) = parse_args(args, &["wait"])?;
            only_options(&options, &["pool", "os", "arch", "job", "wait"])?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {:?}", positional[0]));
            }
            let path = option(&options, "job")?;
            let text = fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
            let description = ci_json::parse(&text).map_err(|e| format!("{path}: {e}"))?;
            let reply = admin.json(
                "POST",
                "/admin/v1/jobs",
                Some(&Value::object([
                    ("pool", Value::from(option(&options, "pool")?)),
                    ("os", Value::from(option(&options, "os")?)),
                    ("arch", Value::from(option(&options, "arch")?)),
                    ("description", description),
                ])),
            )?;
            let id = reply
                .get("job_id")
                .and_then(Value::as_i64)
                .ok_or("the reply has no job id")?;
            if options.iter().any(|(n, _)| n == "wait") {
                eprintln!("job {id} queued");
                wait(admin, id)
            } else {
                println!("{id}");
                Ok(ExitCode::SUCCESS)
            }
        }
        "status" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &[])?;
            let status = admin.json(
                "GET",
                &format!("/admin/v1/jobs/{}", job_id(&positional)?),
                None,
            )?;
            print!("{}", status.to_json_pretty());
            Ok(ExitCode::SUCCESS)
        }
        "log" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &[])?;
            admin.print_log(job_id(&positional)?, 0)?;
            Ok(ExitCode::SUCCESS)
        }
        "cancel" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &[])?;
            let status = admin.json(
                "POST",
                &format!("/admin/v1/jobs/{}/cancel", job_id(&positional)?),
                None,
            )?;
            print!("{}", status.to_json_pretty());
            Ok(ExitCode::SUCCESS)
        }
        "artifact" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &["output"])?;
            let id = job_id(&positional)?;
            let name = positional.get(1).ok_or("an artifact path is required")?;
            let output = option(&options, "output")?;
            let encoded: Vec<String> = name.split('/').map(ci_http::percent_encode).collect();
            let (status, mut response) = admin.call(
                "GET",
                &format!("/admin/v1/jobs/{id}/artifacts/{}", encoded.join("/")),
                None,
            )?;
            if status != 200 {
                return Err(format!(
                    "the coordinator answered {status} for that artifact"
                ));
            }
            let mut file =
                fs::File::create(output).map_err(|e| format!("cannot create {output}: {e}"))?;
            io::copy(&mut response, &mut file)
                .map_err(|e| format!("downloading to {output}: {e}"))?;
            Ok(ExitCode::SUCCESS)
        }
        "release" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(
                &options,
                &[
                    "rosdistro-url",
                    "commit",
                    "check-job",
                    "archive-job",
                    "rosdistro",
                    "check-pool",
                    "archive-pool",
                    "os",
                    "arch",
                ],
            )?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {:?}", positional[0]));
            }
            let or = |name: &str, default: &'static str| option(&options, name).unwrap_or(default);
            let commit = option(&options, "commit")?;
            if commit.is_empty() || !commit.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err("--commit must be a commit id".to_string());
            }
            release::release(
                admin,
                &release::Options {
                    rosdistro_url: option(&options, "rosdistro-url")?,
                    commit,
                    rosdistro: or("rosdistro", "rolling"),
                    check_job: option(&options, "check-job")?,
                    archive_job: option(&options, "archive-job")?,
                    check_pool: or("check-pool", "pull-request"),
                    archive_pool: or("archive-pool", "release"),
                    os: or("os", "linux"),
                    arch: or("arch", "amd64"),
                },
            )
        }
        "archived" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &["rosdistro"])?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {:?}", positional[0]));
            }
            let rosdistro = option(&options, "rosdistro").unwrap_or("rolling");
            let archived = admin.json(
                "GET",
                &format!("/admin/v1/archive/{}", ci_http::percent_encode(rosdistro)),
                None,
            )?;
            print!("{}", archived.to_json_pretty());
            Ok(ExitCode::SUCCESS)
        }
        "create-repository" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(
                &options,
                &[
                    "name",
                    "format",
                    "version-suffix",
                    "architectures",
                    "source-arch",
                    "suite",
                ],
            )?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {:?}", positional[0]));
            }
            let mut body = vec![
                ("name", Value::from(option(&options, "name")?)),
                ("format", Value::from(option(&options, "format")?)),
                (
                    "version_suffix",
                    Value::from(option(&options, "version-suffix")?),
                ),
                (
                    "architectures",
                    Value::strings(option(&options, "architectures")?.split(',')),
                ),
                ("source_arch", Value::from(option(&options, "source-arch")?)),
            ];
            if let Ok(suite) = option(&options, "suite") {
                body.push(("suite", Value::from(suite)));
            }
            admin.json("POST", "/admin/v1/repositories", Some(&Value::object(body)))?;
            Ok(ExitCode::SUCCESS)
        }
        "repository" | "repository-key" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &[])?;
            let name = positional.first().ok_or("a repository name is required")?;
            let status = admin.json(
                "GET",
                &format!("/admin/v1/repositories/{}", ci_http::percent_encode(name)),
                None,
            )?;
            if command == "repository" {
                print!("{}", status.to_json_pretty());
            } else {
                print!(
                    "{}",
                    status
                        .get("public_key")
                        .and_then(Value::as_str)
                        .ok_or("the coordinator has no signing key")?
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        "next-revision" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(&options, &["repository", "package", "version"])?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {:?}", positional[0]));
            }
            let reply = admin.json(
                "POST",
                &format!(
                    "/admin/v1/repositories/{}/revisions",
                    ci_http::percent_encode(option(&options, "repository")?)
                ),
                Some(&Value::object([
                    ("package", Value::from(option(&options, "package")?)),
                    ("version", Value::from(option(&options, "version")?)),
                ])),
            )?;
            println!(
                "{}",
                reply
                    .get("revision")
                    .and_then(Value::as_i64)
                    .ok_or("the reply held no revision")?
            );
            Ok(ExitCode::SUCCESS)
        }
        "publish" => {
            let (options, positional) = parse_args(args, &[])?;
            only_options(
                &options,
                &[
                    "repository",
                    "job",
                    "package",
                    "version",
                    "revision",
                    "name",
                ],
            )?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {:?}", positional[0]));
            }
            let number = |name: &str| -> Result<i64, String> {
                option(&options, name)?
                    .parse()
                    .map_err(|_| format!("--{name} needs a whole number"))
            };
            let names = options
                .iter()
                .filter(|(name, _)| name == "name")
                .map(|(_, value)| value.as_str());
            let reply = admin.json(
                "POST",
                &format!(
                    "/admin/v1/repositories/{}/publish",
                    ci_http::percent_encode(option(&options, "repository")?)
                ),
                Some(&Value::object([
                    ("job_id", Value::Int(number("job")?)),
                    ("package", Value::from(option(&options, "package")?)),
                    ("version", Value::from(option(&options, "version")?)),
                    ("revision", Value::Int(number("revision")?)),
                    ("names", Value::strings(names)),
                ])),
            )?;
            for file in reply
                .get("files")
                .and_then(Value::as_array)
                .unwrap_or_default()
            {
                println!("{}", file.as_str().unwrap_or_default());
            }
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown command {other:?}; see ci-admin --help")),
    }
}

/// Makes a signing key and writes it to a new private file.
fn generate_signing_key(args: &[String]) -> Result<(), String> {
    let (options, positional) = parse_args(args, &[])?;
    only_options(&options, &["user-id", "output"])?;
    if !positional.is_empty() {
        return Err(format!("unexpected argument {:?}", positional[0]));
    }
    let output = option(&options, "output")?;
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as u32;
    let key = SigningKey::generate(option(&options, "user-id")?, created)?;
    // Never over an existing file: replacing a key that repositories
    // are signed with would lock their users out.
    let mut open = fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let mut file = open
        .open(output)
        .map_err(|e| format!("cannot create {output}: {e}"))?;
    file.write_all(key.to_file_text().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("cannot write {output}: {e}"))?;
    println!("{}", key.fingerprint_hex());
    Ok(())
}

/// Packs a directory into a tar archive for use as a build context.
/// Files are added in name order with fixed timestamps, so the archive,
/// and therefore its hash, depends only on the directory's contents.
fn pack_context(directory: &std::path::Path, output: &std::path::Path) -> Result<(), String> {
    if !directory.is_dir() {
        return Err(format!("{} is not a directory", directory.display()));
    }
    let file =
        fs::File::create(output).map_err(|e| format!("cannot create {}: {e}", output.display()))?;
    let mut builder = ci_tar::Builder::new(io::BufWriter::new(file));
    builder
        .append_tree(directory, "")
        .map_err(|e| format!("packing {}: {e}", directory.display()))?;
    builder
        .finish()
        .map_err(|e| e.to_string())?
        .flush()
        .map_err(|e| e.to_string())
}

/// Follows a job until it finishes, printing its log as it grows.
fn wait(admin: &Admin, id: i64) -> Result<ExitCode, String> {
    let mut shown = 0u64;
    let mut attempt = 0i64;
    loop {
        let status = admin.json("GET", &format!("/admin/v1/jobs/{id}"), None)?;
        let current = status.get("attempt").and_then(Value::as_i64).unwrap_or(0);
        if current != attempt {
            if attempt != 0 {
                eprintln!("job {id}: attempt {attempt} did not finish; starting attempt {current}");
            }
            // A new attempt has a new log.
            attempt = current;
            shown = 0;
        }
        if status.get("log_bytes").and_then(Value::as_u64).unwrap_or(0) > shown {
            shown = admin.print_log(id, shown)?;
        }
        if status.get("state").and_then(Value::as_str) == Some("finished") {
            let outcome = status
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let message = status.get("message").and_then(Value::as_str).unwrap_or("");
            eprintln!(
                "job {id}: {outcome}{}",
                if message.is_empty() {
                    String::new()
                } else {
                    format!(" ({message})")
                }
            );
            return Ok(match outcome {
                "success" => ExitCode::SUCCESS,
                "infrastructure_failure" => ExitCode::from(2),
                _ => ExitCode::from(1),
            });
        }
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn argument_parsing() {
        let (options, positional) = parse_args(
            &strings(&["12", "--pool", "release", "--wait", "out/x.txt"]),
            &["wait"],
        )
        .unwrap();
        assert_eq!(positional, ["12", "out/x.txt"]);
        assert_eq!(option(&options, "pool").unwrap(), "release");
        assert!(options.iter().any(|(n, _)| n == "wait"));
        assert!(option(&options, "os").is_err());
        assert!(parse_args(&strings(&["--pool"]), &[]).is_err());
        assert!(only_options(&options, &["pool", "wait"]).is_ok());
        assert!(only_options(&options, &["pool"]).is_err());
    }

    #[test]
    fn packing_a_context_depends_only_on_contents() {
        let base = std::env::temp_dir().join(format!("ci_admin_pack_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let make = |name: &str, order: &[(&str, &str)]| {
            let dir = base.join(name);
            for (path, contents) in order {
                let file = dir.join(path);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(file, contents).unwrap();
            }
            let out = base.join(format!("{name}.tar"));
            pack_context(&dir, &out).unwrap();
            fs::read(out).unwrap()
        };
        // The same files, created in a different order, at different times.
        let first = make(
            "one",
            &[("Dockerfile", "FROM x\n"), ("scripts/run.sh", "echo\n")],
        );
        std::thread::sleep(Duration::from_millis(1100));
        let second = make(
            "two",
            &[("scripts/run.sh", "echo\n"), ("Dockerfile", "FROM x\n")],
        );
        assert_eq!(first, second);
        let changed = make(
            "three",
            &[("Dockerfile", "FROM y\n"), ("scripts/run.sh", "echo\n")],
        );
        assert_ne!(first, changed);
        assert!(pack_context(&base.join("missing"), &base.join("x.tar")).is_err());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn job_ids() {
        assert_eq!(job_id(&strings(&["42"])).unwrap(), 42);
        for bad in [&[][..], &["x"], &["0"], &["-3"], &["1.5"]] {
            assert!(job_id(&strings(bad)).is_err(), "{bad:?}");
        }
    }
}
