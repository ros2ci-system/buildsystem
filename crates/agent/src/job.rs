// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The job description: everything the agent needs to run one job.
//!
//! The agent knows nothing about ROS, packages or pull requests. A job
//! description names an image, a command, inputs and a timeout, and the
//! agent runs it and reports what happened.

use std::path::PathBuf;
use std::time::Duration;

use ci_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct JobDescription {
    /// The image to run, which should be named by digest.
    pub image: String,
    pub command: Vec<String>,
    pub environment: Vec<(String, String)>,
    /// Written as the parameters file in the container, unchanged.
    pub parameters: Value,
    pub inputs: Vec<Input>,
    pub timeout: Duration,
}

/// A file copied into the job's input directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// The file's name inside the input directory.
    pub name: String,
    pub source: InputSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputSource {
    /// A file on the agent's own disk, from a `file://` URL.
    LocalFile(PathBuf),
}

impl JobDescription {
    pub fn parse(text: &str) -> Result<JobDescription, String> {
        let doc = ci_json::parse(text).map_err(|e| e.to_string())?;
        if doc.as_object().is_none() {
            return Err("the job description must be a JSON object".to_string());
        }
        for (key, _) in doc.as_object().unwrap_or(&[]) {
            if ![
                "image",
                "command",
                "environment",
                "parameters",
                "inputs",
                "timeout_seconds",
            ]
            .contains(&key.as_str())
            {
                return Err(format!("unknown member {key:?}"));
            }
        }

        let image = doc
            .get("image")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("\"image\" must be a non-empty string")?
            .to_string();

        let command: Vec<String> = doc
            .get("command")
            .and_then(Value::as_array)
            .ok_or("\"command\" must be an array of strings")?
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or("\"command\" must be an array of strings")?;
        if command.is_empty() {
            return Err("\"command\" must not be empty".to_string());
        }

        let mut environment = Vec::new();
        if let Some(env) = doc.get("environment") {
            let members = env
                .as_object()
                .ok_or("\"environment\" must be an object of strings")?;
            for (name, value) in members {
                let value = value
                    .as_str()
                    .ok_or("\"environment\" must be an object of strings")?;
                if name.is_empty() || name.contains('=') || name.contains('\0') {
                    return Err(format!("bad environment variable name {name:?}"));
                }
                environment.push((name.clone(), value.to_string()));
            }
        }

        let parameters = doc
            .get("parameters")
            .cloned()
            .unwrap_or(Value::Object(Vec::new()));

        let mut inputs: Vec<Input> = Vec::new();
        if let Some(list) = doc.get("inputs") {
            for item in list.as_array().ok_or("\"inputs\" must be an array")? {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("each input needs a \"name\"")?;
                if !is_plain_file_name(name) {
                    return Err(format!("input name {name:?} must be a plain file name"));
                }
                if inputs.iter().any(|i| i.name.eq_ignore_ascii_case(name)) {
                    return Err(format!("input name {name:?} is used twice"));
                }
                let url = item
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or("each input needs a \"url\"")?;
                let source = match url.strip_prefix("file://") {
                    Some(path) if !path.is_empty() => InputSource::LocalFile(PathBuf::from(path)),
                    _ => {
                        return Err(format!(
                            "input URL {url:?} is not supported; only file:// is"
                        ))
                    }
                };
                inputs.push(Input {
                    name: name.to_string(),
                    source,
                });
            }
        }

        let timeout = match doc.get("timeout_seconds").and_then(Value::as_u64) {
            Some(seconds) if seconds > 0 => Duration::from_secs(seconds),
            _ => return Err("\"timeout_seconds\" must be a positive whole number".to_string()),
        };

        Ok(JobDescription {
            image,
            command,
            environment,
            parameters,
            inputs,
            timeout,
        })
    }
}

/// A name that is safe to use as one file in a directory, on Linux and on
/// Windows.
fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{"image": "alpine", "command": ["true"], "timeout_seconds": 60}"#;

    #[test]
    fn minimal() {
        let job = JobDescription::parse(MINIMAL).unwrap();
        assert_eq!(job.image, "alpine");
        assert_eq!(job.command, ["true"]);
        assert!(job.environment.is_empty());
        assert_eq!(job.parameters, Value::Object(vec![]));
        assert!(job.inputs.is_empty());
        assert_eq!(job.timeout, Duration::from_secs(60));
    }

    #[test]
    fn full() {
        let job = JobDescription::parse(
            r#"{
                "image": "registry.example/core@sha256:abcd",
                "command": ["/job-scripts/run", "--verbose"],
                "environment": {"A": "1", "B": "two words"},
                "parameters": {"distribution": "rolling", "repos": [1, 2]},
                "inputs": [{"name": "src.tar", "url": "file:///tmp/src.tar"}],
                "timeout_seconds": 7200
            }"#,
        )
        .unwrap();
        assert_eq!(
            job.environment,
            [("A".into(), "1".into()), ("B".into(), "two words".into())]
        );
        assert_eq!(
            job.parameters.get("distribution").unwrap().as_str(),
            Some("rolling")
        );
        assert_eq!(
            job.inputs,
            [Input {
                name: "src.tar".into(),
                source: InputSource::LocalFile("/tmp/src.tar".into())
            }]
        );
        assert_eq!(job.timeout, Duration::from_secs(7200));
    }

    fn with(member: &str) -> String {
        format!(r#"{{"image": "alpine", "command": ["true"], "timeout_seconds": 60, {member}}}"#)
    }

    #[test]
    fn rejects_bad_descriptions() {
        for bad in [
            "[]",
            "{}",
            r#"{"image": "", "command": ["true"], "timeout_seconds": 60}"#,
            r#"{"image": "a", "command": [], "timeout_seconds": 60}"#,
            r#"{"image": "a", "command": "true", "timeout_seconds": 60}"#,
            r#"{"image": "a", "command": ["true", 1], "timeout_seconds": 60}"#,
            r#"{"image": "a", "command": ["true"]}"#,
            r#"{"image": "a", "command": ["true"], "timeout_seconds": 0}"#,
            r#"{"image": "a", "command": ["true"], "timeout_seconds": -5}"#,
            r#"{"image": "a", "command": ["true"], "timeout_seconds": 1.5}"#,
            &with(r#""surprise": 1"#),
            &with(r#""environment": {"A=B": "x"}"#),
            &with(r#""environment": {"A": 1}"#),
            &with(r#""environment": []"#),
            &with(r#""inputs": [{"name": "a"}]"#),
            &with(r#""inputs": [{"url": "file:///a"}]"#),
            &with(r#""inputs": [{"name": "a", "url": "https://example.com/a"}]"#),
            &with(r#""inputs": [{"name": "a", "url": "file://"}]"#),
            &with(r#""inputs": [{"name": "../a", "url": "file:///a"}]"#),
            &with(r#""inputs": [{"name": "a/b", "url": "file:///a"}]"#),
            &with(r#""inputs": [{"name": "a\\b", "url": "file:///a"}]"#),
            &with(r#""inputs": [{"name": "..", "url": "file:///a"}]"#),
            &with(
                r#""inputs": [{"name": "a", "url": "file:///a"}, {"name": "A", "url": "file:///b"}]"#,
            ),
        ] {
            assert!(JobDescription::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn plain_file_names() {
        for good in ["a", "src.tar.gz", "a_b-c+d.1"] {
            assert!(is_plain_file_name(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a:b",
            "a b",
            "trailing.",
            "é",
            "con\0",
        ] {
            assert!(!is_plain_file_name(bad), "{bad:?}");
        }
    }
}
