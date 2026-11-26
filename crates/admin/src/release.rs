// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! `ci-admin release`: takes the releases of a rosdistro commit into the
//! system.
//!
//! It runs the release check as a job, and then one source archive job
//! for each release the archive does not hold yet. The jobs themselves
//! are described by two files from the configuration; this command adds
//! their parameters, waits for them, and tells the coordinator to take
//! each tarball into the archive.

use std::fs;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use ci_json::Value;

use crate::Admin;

pub struct Options<'a> {
    /// Where the rosdistro is, without the commit.
    pub rosdistro_url: &'a str,
    pub commit: &'a str,
    pub rosdistro: &'a str,
    pub check_job: &'a str,
    pub archive_job: &'a str,
    pub check_pool: &'a str,
    pub archive_pool: &'a str,
    pub os: &'a str,
    pub arch: &'a str,
}

/// A job description from a file, with members added to its parameters.
fn job_with(path: &str, parameters: Vec<(&str, Value)>) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let Value::Object(mut members) = ci_json::parse(&text).map_err(|e| format!("{path}: {e}"))?
    else {
        return Err(format!("{path}: a job description is a JSON object"));
    };
    let mut merged = match members.iter().position(|(name, _)| name == "parameters") {
        Some(index) => match members.remove(index).1 {
            Value::Object(existing) => existing,
            _ => return Err(format!("{path}: \"parameters\" must be an object")),
        },
        None => Vec::new(),
    };
    for (name, value) in parameters {
        merged.retain(|(existing, _)| existing != name);
        merged.push((name.to_string(), value));
    }
    members.push(("parameters".to_string(), Value::Object(merged)));
    Ok(Value::Object(members))
}

/// Queues a job and waits for it. Returns its id and whether it
/// succeeded.
fn run_job(
    admin: &Admin,
    pool: &str,
    options: &Options<'_>,
    description: Value,
) -> Result<(i64, bool), String> {
    let reply = admin.json(
        "POST",
        "/admin/v1/jobs",
        Some(&Value::object([
            ("pool", Value::from(pool)),
            ("os", Value::from(options.os)),
            ("arch", Value::from(options.arch)),
            ("description", description),
        ])),
    )?;
    let id = reply
        .get("job_id")
        .and_then(Value::as_i64)
        .ok_or("the reply has no job id")?;
    loop {
        let status = admin.json("GET", &format!("/admin/v1/jobs/{id}"), None)?;
        if status.get("state").and_then(Value::as_str) == Some("finished") {
            let outcome = status.get("outcome").and_then(Value::as_str);
            return Ok((id, outcome == Some("success")));
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn text<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("")
}

fn list<'a>(value: &'a Value, name: &str) -> &'a [Value] {
    value
        .get(name)
        .and_then(Value::as_array)
        .unwrap_or_default()
}

pub fn release(admin: &Admin, options: &Options<'_>) -> Result<ExitCode, String> {
    let rosdistro_url = format!(
        "{}/{}",
        options.rosdistro_url.trim_end_matches('/'),
        options.commit
    );
    let archive_path = format!(
        "/admin/v1/archive/{}",
        ci_http::percent_encode(options.rosdistro)
    );

    // What the archive holds decides which releases are new.
    let archived = admin.json("GET", &archive_path, None)?;
    let known: Vec<Value> = list(&archived, "releases")
        .iter()
        .map(|release| {
            Value::object([
                ("repository", Value::from(text(release, "repository"))),
                ("version", Value::from(text(release, "version"))),
                ("commit", Value::from(text(release, "commit"))),
                ("url", Value::from(text(release, "url"))),
                ("tag", Value::from(text(release, "tag"))),
                ("packages", Value::Array(list(release, "packages").to_vec())),
                ("skipped", Value::Array(list(release, "skipped").to_vec())),
            ])
        })
        .collect();

    let check = job_with(
        options.check_job,
        vec![
            ("job", Value::from("release_check")),
            ("rosdistro", Value::from(options.rosdistro)),
            ("rosdistro_url", Value::from(rosdistro_url.as_str())),
            ("archived", Value::Array(known)),
        ],
    )?;
    let (check_id, _) = run_job(admin, options.check_pool, options, check)?;
    let (status, mut response) = admin.call(
        "GET",
        &format!("/admin/v1/jobs/{check_id}/artifacts/results.json"),
        None,
    )?;
    if status != 200 {
        return Err(format!(
            "the release check, job {check_id}, left no results; see its log"
        ));
    }
    let results = ci_json::parse_bytes(&response.body_bytes(64 << 20).map_err(|e| e.to_string())?)
        .map_err(|e| format!("the results of job {check_id}: {e}"))?;

    println!(
        "release check: job {check_id}, {}",
        text(&results, "status")
    );
    for release in list(&results, "releases") {
        println!(
            "  {:<10} {} {} ({} packages)",
            text(release, "state"),
            text(release, "repository"),
            text(release, "version"),
            list(release, "packages").len()
        );
    }
    for moved in list(&results, "moved_tags") {
        let now = match text(moved, "commit_now") {
            "" => "is gone".to_string(),
            commit => format!("names {commit}"),
        };
        println!(
            "MOVED TAG: {} {}: tag {} was archived as commit {} and now {now}. Builds keep reading the archive.",
            text(moved, "repository"),
            text(moved, "version"),
            text(moved, "tag"),
            text(moved, "archived_commit"),
        );
    }
    for problem in list(&results, "problems") {
        let package = match text(problem, "package") {
            "" => String::new(),
            name => format!(" ({name})"),
        };
        println!(
            "PROBLEM: {}{package}: {}",
            text(problem, "repository"),
            text(problem, "message")
        );
    }
    for warning in list(&results, "warnings") {
        println!("warning: {}", warning.as_str().unwrap_or_default());
    }
    if text(&results, "status") != "success" {
        println!("nothing was archived: {}", text(&results, "message"));
        return Ok(ExitCode::from(1));
    }

    let mut failed = false;
    for release in list(&results, "releases") {
        if text(release, "state") != "new" {
            continue;
        }
        let (repository, version) = (text(release, "repository"), text(release, "version"));
        let named = ["repository", "url", "version", "tag", "commit"];
        let mut parameters = vec![
            ("job", Value::from("source_archive")),
            ("rosdistro", Value::from(options.rosdistro)),
            ("rosdistro_url", Value::from(rosdistro_url.as_str())),
            ("skip", Value::Array(list(release, "skipped").to_vec())),
        ];
        for name in named {
            parameters.push((name, Value::from(text(release, name))));
        }
        let (job_id, succeeded) = run_job(
            admin,
            options.archive_pool,
            options,
            job_with(options.archive_job, parameters)?,
        )?;
        if !succeeded {
            println!(
                "FAILED: {repository} {version}: the source archive job, {job_id}, did not succeed"
            );
            failed = true;
            continue;
        }
        let mut body = vec![
            ("job_id", Value::Int(job_id)),
            ("rosdistro", Value::from(options.rosdistro)),
        ];
        for name in named {
            body.push((name, Value::from(text(release, name))));
        }
        match admin.json("POST", "/admin/v1/archive", Some(&Value::object(body))) {
            Ok(record) => println!(
                "archived: {repository} {version}, commit {}, as {} (job {job_id})",
                text(&record, "commit"),
                text(&record, "sha256")
            ),
            Err(message) => {
                println!("FAILED: {repository} {version}: {message}");
                failed = true;
            }
        }
    }
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_are_added_to_a_job_description() {
        let path = std::env::temp_dir().join(format!("ci-admin-job-{}.json", std::process::id()));
        fs::write(
            &path,
            r#"{"image": "x", "parameters": {"job": "old", "rosdistro": "rolling"}, "timeout_seconds": 5}"#,
        )
        .unwrap();
        let job = job_with(
            path.to_str().unwrap(),
            vec![
                ("job", Value::from("release_check")),
                ("commit", Value::from("abc")),
            ],
        )
        .unwrap();
        let parameters = job.get("parameters").unwrap();
        assert_eq!(text(parameters, "job"), "release_check");
        assert_eq!(text(parameters, "rosdistro"), "rolling");
        assert_eq!(text(parameters, "commit"), "abc");
        assert_eq!(job.get("timeout_seconds").and_then(Value::as_i64), Some(5));
        fs::write(&path, "[1]").unwrap();
        assert!(job_with(path.to_str().unwrap(), Vec::new()).is_err());
        fs::remove_file(&path).unwrap();
    }
}
