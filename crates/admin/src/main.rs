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

const USAGE: &str = "\
Usage: ci-admin [--admin-url <url>] [--token-file <file>] <command>

Commands:
  create-worker-token --name <text> --pool <pool>
      Issues an enrolment token for a worker and prints it. The token is
      shown once and cannot be recovered.
  revoke-worker-token <token-id>
      Revokes a worker's enrolment token. Workers that registered with it
      are refused from then on.
  create-admin-token --name <text>
      Issues another admin token and prints it. Give each person or
      service its own, so that the coordinator's log says who did what.
  revoke-admin-token <token-id>
      Revokes an admin token.
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
        other => Err(format!("unknown command {other:?}; see ci-admin --help")),
    }
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
    fn job_ids() {
        assert_eq!(job_id(&strings(&["42"])).unwrap(), 42);
        for bad in [&[][..], &["x"], &["0"], &["-3"], &["1.5"]] {
            assert!(job_id(&strings(bad)).is_err(), "{bad:?}");
        }
    }
}
