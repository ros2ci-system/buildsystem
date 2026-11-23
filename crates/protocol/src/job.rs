// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The job description: everything the agent needs to run one job.
//!
//! The agent knows nothing about ROS, packages or pull requests. A job
//! description names an image, a command, inputs and a timeout, and the
//! agent runs it and reports what happened.
//!
//! An image is named in one of two ways. A **reference** is something a
//! registry can supply, ideally by digest. A **build definition** is the
//! recipe itself: a build context and the path of the build file in it.
//! A worker that has not yet built that recipe builds it and keeps the
//! result, so images work with no registry at all.

use std::path::PathBuf;
use std::time::Duration;

use ci_crypto::{hex_encode, sha256};
use ci_json::Value;

/// The prefix of a URL that names a file stored on the coordinator.
pub const STORED_PREFIX: &str = "stored:";

/// How long an image build may take when the job does not say.
const DEFAULT_BUILD_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

#[derive(Debug, Clone, PartialEq)]
pub struct JobDescription {
    pub kind: JobKind,
    pub image: Image,
    /// What to run in the container. Empty for an image build job.
    pub command: Vec<String>,
    pub environment: Vec<(String, String)>,
    /// Written as the parameters file in the container, unchanged.
    pub parameters: Value,
    pub inputs: Vec<Input>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    /// Run a command in a container.
    Run,
    /// Build the job's image and do nothing else.
    ImageBuild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    /// An image a registry can supply.
    Reference(String),
    /// An image the worker builds if it does not already have it.
    Build(BuildDefinition),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDefinition {
    /// A tar archive holding the build file and what it copies in.
    pub context: Source,
    /// The SHA-256 of the context, in hex. A description sent to a worker
    /// always has it: the coordinator fills it in for a stored context.
    pub context_sha256: Option<String>,
    /// The build file's path inside the context.
    pub build_file: String,
    /// Raised to force a rebuild of an unchanged recipe, for example
    /// when the base image has moved on too far.
    pub generation: u64,
}

impl BuildDefinition {
    /// The name the built image is kept under on a worker. It depends on
    /// everything that decides what gets built, so a change to any of
    /// them is a different image, and the same recipe is built once.
    /// Returns `None` until the context's hash is known.
    pub fn local_tag(&self) -> Option<String> {
        let hash = self.context_sha256.as_deref()?;
        let identity = format!(
            "{}\n{}\n{}",
            hash.to_ascii_lowercase(),
            self.build_file,
            self.generation
        );
        Some(format!(
            "ros2ci-cache/image:{}",
            hex_encode(&sha256(identity.as_bytes()))
        ))
    }
}

/// A file copied into the job's input directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// The file's name inside the input directory.
    pub name: String,
    pub source: Source,
}

/// Where a file comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A file on the agent's own disk, from a `file://` URL. Only for
    /// jobs run by hand on the agent's machine: the coordinator refuses
    /// job descriptions that use it.
    LocalFile(PathBuf),
    /// An `http://` or `https://` URL the agent downloads.
    Url(String),
    /// A file stored on the coordinator under this name, from a
    /// `stored:` URL. The worker running the job may download it.
    Stored(String),
}

impl Source {
    fn parse(url: &str) -> Result<Source, String> {
        if let Some(path) = url.strip_prefix("file://") {
            if path.is_empty() {
                return Err(format!("URL {url:?} has no path"));
            }
            Ok(Source::LocalFile(PathBuf::from(path)))
        } else if let Some(name) = url.strip_prefix(STORED_PREFIX) {
            if !is_plain_file_name(name) {
                return Err(format!(
                    "stored file name {name:?} must be a plain file name"
                ));
            }
            Ok(Source::Stored(name.to_string()))
        } else if url.starts_with("http://") || url.starts_with("https://") {
            Ok(Source::Url(url.to_string()))
        } else {
            Err(format!(
                "URL {url:?} must start with http://, https://, stored: or file://"
            ))
        }
    }
}

impl JobDescription {
    /// Every source the job reads: its inputs and its build context.
    pub fn sources(&self) -> impl Iterator<Item = &Source> {
        let context = match &self.image {
            Image::Build(build) => Some(&build.context),
            Image::Reference(_) => None,
        };
        self.inputs.iter().map(|input| &input.source).chain(context)
    }

    /// Whether anything is read from the agent's own disk.
    pub fn has_local_inputs(&self) -> bool {
        self.sources()
            .any(|source| matches!(source, Source::LocalFile(_)))
    }

    /// The names of the stored files the job reads.
    pub fn stored_files(&self) -> Vec<&str> {
        self.sources()
            .filter_map(|source| match source {
                Source::Stored(name) => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    /// How long building the job's image may take.
    pub fn build_timeout(&self) -> Duration {
        match self.kind {
            JobKind::ImageBuild => self.timeout,
            JobKind::Run => DEFAULT_BUILD_TIMEOUT,
        }
    }

    pub fn parse(text: &str) -> Result<JobDescription, String> {
        let doc = ci_json::parse(text).map_err(|e| e.to_string())?;
        if doc.as_object().is_none() {
            return Err("the job description must be a JSON object".to_string());
        }
        for (key, _) in doc.as_object().unwrap_or(&[]) {
            if ![
                "kind",
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

        let kind = match doc.get("kind").map(|v| v.as_str()) {
            None | Some(Some("run")) => JobKind::Run,
            Some(Some("image_build")) => JobKind::ImageBuild,
            Some(_) => return Err("\"kind\" must be \"run\" or \"image_build\"".to_string()),
        };

        let image = parse_image(doc.get("image").ok_or("\"image\" is required")?)?;
        if kind == JobKind::ImageBuild && !matches!(image, Image::Build(_)) {
            return Err("an image build job needs \"image\" to be a build definition".to_string());
        }

        let command: Vec<String> = match doc.get("command") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .ok_or("\"command\" must be an array of strings")?
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<_>>()
                .ok_or("\"command\" must be an array of strings")?,
        };
        match kind {
            JobKind::Run if command.is_empty() => {
                return Err("\"command\" must not be empty".to_string())
            }
            JobKind::ImageBuild if !command.is_empty() => {
                return Err("an image build job runs no command".to_string())
            }
            _ => {}
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
                inputs.push(Input {
                    name: name.to_string(),
                    source: Source::parse(url).map_err(|e| format!("input {name}: {e}"))?,
                });
            }
        }
        if kind == JobKind::ImageBuild && (!inputs.is_empty() || !environment.is_empty()) {
            return Err("an image build job takes no inputs or environment".to_string());
        }

        let timeout = match doc.get("timeout_seconds").and_then(Value::as_u64) {
            Some(seconds) if seconds > 0 => Duration::from_secs(seconds),
            _ => return Err("\"timeout_seconds\" must be a positive whole number".to_string()),
        };

        Ok(JobDescription {
            kind,
            image,
            command,
            environment,
            parameters,
            inputs,
            timeout,
        })
    }
}

fn parse_image(value: &Value) -> Result<Image, String> {
    if let Some(reference) = value.as_str() {
        if reference.is_empty() {
            return Err("\"image\" must not be empty".to_string());
        }
        return Ok(Image::Reference(reference.to_string()));
    }
    let members = value
        .as_object()
        .ok_or("\"image\" must be a string or an object with a \"build\" member")?;
    if members.len() != 1 || members[0].0 != "build" {
        return Err("an \"image\" object must have exactly one member, \"build\"".to_string());
    }
    let build = &members[0].1;
    for (key, _) in build.as_object().ok_or("\"build\" must be an object")? {
        if !["context", "context_sha256", "build_file", "generation"].contains(&key.as_str()) {
            return Err(format!("unknown member {key:?} in \"build\""));
        }
    }
    let context = Source::parse(
        build
            .get("context")
            .and_then(Value::as_str)
            .ok_or("\"build\" needs a \"context\" URL")?,
    )
    .map_err(|e| format!("build context: {e}"))?;
    let context_sha256 = match build.get("context_sha256") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let hash = value.as_str().unwrap_or("");
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("\"context_sha256\" must be 64 hex digits".to_string());
            }
            Some(hash.to_ascii_lowercase())
        }
    };
    // A context fetched from elsewhere could change under the same URL.
    // Its hash pins what is built, and names the result.
    if matches!(context, Source::Url(_) | Source::LocalFile(_)) && context_sha256.is_none() {
        return Err("a build context from a URL needs \"context_sha256\"".to_string());
    }
    let build_file = build
        .get("build_file")
        .and_then(Value::as_str)
        .ok_or("\"build\" needs a \"build_file\" path")?;
    if !is_relative_path(build_file) {
        return Err(format!(
            "build file path {build_file:?} must be a relative path inside the context"
        ));
    }
    let generation = match build.get("generation") {
        None => 0,
        Some(value) => value
            .as_u64()
            .ok_or("\"generation\" must be a whole number")?,
    };
    Ok(Image::Build(BuildDefinition {
        context,
        context_sha256,
        build_file: build_file.to_string(),
        generation,
    }))
}

/// Sets the hash of a stored build context in a job description's JSON.
/// The coordinator does this when it hands a job to a worker, so that the
/// worker knows which image the recipe makes without downloading it.
pub fn set_context_sha256(description: &mut Value, sha256: &str) {
    let Value::Object(members) = description else {
        return;
    };
    let Some((_, Value::Object(image))) = members.iter_mut().find(|(k, _)| k == "image") else {
        return;
    };
    let Some((_, Value::Object(build))) = image.iter_mut().find(|(k, _)| k == "build") else {
        return;
    };
    build.retain(|(k, _)| k != "context_sha256");
    build.push(("context_sha256".to_string(), Value::from(sha256)));
}

/// A name that is safe to use as one file in a directory, on Linux and on
/// Windows.
pub fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
}

/// A path made of plain file names separated by `/`.
fn is_relative_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= 1024 && path.split('/').all(is_plain_file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{"image": "alpine", "command": ["true"], "timeout_seconds": 60}"#;
    const HASH: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn minimal() {
        let job = JobDescription::parse(MINIMAL).unwrap();
        assert_eq!(job.kind, JobKind::Run);
        assert_eq!(job.image, Image::Reference("alpine".into()));
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
                "kind": "run",
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
                source: Source::LocalFile("/tmp/src.tar".into())
            }]
        );
        assert_eq!(job.timeout, Duration::from_secs(7200));
    }

    fn with(member: &str) -> String {
        format!(r#"{{"image": "alpine", "command": ["true"], "timeout_seconds": 60, {member}}}"#)
    }

    fn built(build: &str, rest: &str) -> String {
        format!(r#"{{"image": {{"build": {build}}}, "timeout_seconds": 60{rest}}}"#)
    }

    #[test]
    fn rejects_bad_descriptions() {
        for bad in [
            "[]",
            "{}",
            r#"{"image": "", "command": ["true"], "timeout_seconds": 60}"#,
            r#"{"image": 5, "command": ["true"], "timeout_seconds": 60}"#,
            r#"{"command": ["true"], "timeout_seconds": 60}"#,
            r#"{"image": "a", "command": [], "timeout_seconds": 60}"#,
            r#"{"image": "a", "timeout_seconds": 60}"#,
            r#"{"image": "a", "command": "true", "timeout_seconds": 60}"#,
            r#"{"image": "a", "command": ["true", 1], "timeout_seconds": 60}"#,
            r#"{"image": "a", "command": ["true"]}"#,
            r#"{"image": "a", "command": ["true"], "timeout_seconds": 0}"#,
            r#"{"image": "a", "command": ["true"], "timeout_seconds": -5}"#,
            r#"{"image": "a", "command": ["true"], "timeout_seconds": 1.5}"#,
            &with(r#""surprise": 1"#),
            &with(r#""kind": "deploy""#),
            &with(r#""kind": 3"#),
            &with(r#""environment": {"A=B": "x"}"#),
            &with(r#""environment": {"A": 1}"#),
            &with(r#""environment": []"#),
            &with(r#""inputs": [{"name": "a"}]"#),
            &with(r#""inputs": [{"url": "file:///a"}]"#),
            &with(r#""inputs": [{"name": "a", "url": "ftp://example.com/a"}]"#),
            &with(r#""inputs": [{"name": "a", "url": "file://"}]"#),
            &with(r#""inputs": [{"name": "a", "url": "stored:"}]"#),
            &with(r#""inputs": [{"name": "a", "url": "stored:../x"}]"#),
            &with(r#""inputs": [{"name": "a", "url": "stored:a/b"}]"#),
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
    fn sources() {
        let job = JobDescription::parse(&with(
            r#""inputs": [{"name": "a", "url": "https://example.com/a"}, {"name": "b", "url": "http://h/b"},
                          {"name": "c", "url": "stored:license.dat"}]"#,
        ))
        .unwrap();
        assert_eq!(
            job.inputs[0].source,
            Source::Url("https://example.com/a".into())
        );
        assert_eq!(job.inputs[1].source, Source::Url("http://h/b".into()));
        assert_eq!(job.inputs[2].source, Source::Stored("license.dat".into()));
        assert!(!job.has_local_inputs());
        assert_eq!(job.stored_files(), ["license.dat"]);

        let job = JobDescription::parse(&with(
            r#""inputs": [{"name": "a", "url": "file:///tmp/a"}]"#,
        ))
        .unwrap();
        assert!(job.has_local_inputs());
        assert!(job.stored_files().is_empty());
    }

    #[test]
    fn build_definitions() {
        let job = JobDescription::parse(&built(
            r#"{"context": "stored:context.tar", "build_file": "images/ubuntu/Dockerfile"}"#,
            r#", "command": ["true"]"#,
        ))
        .unwrap();
        let Image::Build(build) = &job.image else {
            panic!("expected a build definition")
        };
        assert_eq!(build.context, Source::Stored("context.tar".into()));
        assert_eq!(build.context_sha256, None);
        assert_eq!(build.build_file, "images/ubuntu/Dockerfile");
        assert_eq!(build.generation, 0);
        assert_eq!(build.local_tag(), None, "no tag until the hash is known");
        assert_eq!(job.stored_files(), ["context.tar"]);
        assert_eq!(job.build_timeout(), Duration::from_secs(7200));

        let job = JobDescription::parse(&built(
            &format!(
                r#"{{"context": "https://example.com/c.tar", "context_sha256": "{}", "build_file": "Dockerfile", "generation": 3}}"#,
                HASH.to_ascii_uppercase()
            ),
            r#", "kind": "image_build""#,
        ))
        .unwrap();
        assert_eq!(job.kind, JobKind::ImageBuild);
        assert!(job.command.is_empty());
        assert_eq!(job.build_timeout(), Duration::from_secs(60));
        let Image::Build(build) = &job.image else {
            panic!("expected a build definition")
        };
        assert_eq!(
            build.context_sha256.as_deref(),
            Some(HASH),
            "the hash is kept in lower case"
        );
        assert_eq!(build.generation, 3);
        let tag = build.local_tag().unwrap();
        assert!(tag.starts_with("ros2ci-cache/image:"), "{tag}");
        assert_eq!(tag.len(), "ros2ci-cache/image:".len() + 64);
    }

    #[test]
    fn local_tags_follow_everything_that_decides_the_image() {
        let base = BuildDefinition {
            context: Source::Stored("c.tar".into()),
            context_sha256: Some(HASH.into()),
            build_file: "Dockerfile".into(),
            generation: 0,
        };
        let tag = base.local_tag().unwrap();
        // The same recipe from a different place is the same image.
        let moved = BuildDefinition {
            context: Source::Url("https://elsewhere/c.tar".into()),
            ..base.clone()
        };
        assert_eq!(moved.local_tag().unwrap(), tag);
        // A different context, build file or generation is not.
        let other_hash = BuildDefinition {
            context_sha256: Some("0".repeat(64)),
            ..base.clone()
        };
        let other_file = BuildDefinition {
            build_file: "other/Dockerfile".into(),
            ..base.clone()
        };
        let next_generation = BuildDefinition {
            generation: 1,
            ..base.clone()
        };
        for different in [other_hash, other_file, next_generation] {
            assert_ne!(different.local_tag().unwrap(), tag, "{different:?}");
        }
    }

    #[test]
    fn rejects_bad_build_definitions() {
        let ok = r#"{"context": "stored:c.tar", "build_file": "Dockerfile"}"#;
        assert!(JobDescription::parse(&built(ok, r#", "command": ["true"]"#)).is_ok());
        assert!(JobDescription::parse(&built(ok, r#", "kind": "image_build""#)).is_ok());
        for (build, rest) in [
            // An image build job with a command, inputs or environment.
            (ok, r#", "kind": "image_build", "command": ["true"]"#),
            (
                ok,
                r#", "kind": "image_build", "inputs": [{"name": "a", "url": "stored:a"}]"#,
            ),
            (ok, r#", "kind": "image_build", "environment": {"A": "b"}"#),
            // A run job with no command.
            (ok, ""),
            (
                r#"{"build_file": "Dockerfile"}"#,
                r#", "command": ["true"]"#,
            ),
            (r#"{"context": "stored:c.tar"}"#, r#", "command": ["true"]"#),
            (
                r#"{"context": "stored:c.tar", "build_file": "/abs/Dockerfile"}"#,
                r#", "command": ["true"]"#,
            ),
            (
                r#"{"context": "stored:c.tar", "build_file": "../Dockerfile"}"#,
                r#", "command": ["true"]"#,
            ),
            (
                r#"{"context": "stored:c.tar", "build_file": "a\\Dockerfile"}"#,
                r#", "command": ["true"]"#,
            ),
            (
                r#"{"context": "stored:c.tar", "build_file": ""}"#,
                r#", "command": ["true"]"#,
            ),
            (
                r#"{"context": "stored:c.tar", "build_file": "Dockerfile", "extra": 1}"#,
                r#", "command": ["true"]"#,
            ),
            (
                r#"{"context": "stored:c.tar", "build_file": "Dockerfile", "generation": -1}"#,
                r#", "command": ["true"]"#,
            ),
            (
                r#"{"context": "stored:c.tar", "build_file": "Dockerfile", "context_sha256": "abc"}"#,
                r#", "command": ["true"]"#,
            ),
            // A context from a URL must be pinned by its hash.
            (
                r#"{"context": "https://h/c.tar", "build_file": "Dockerfile"}"#,
                r#", "command": ["true"]"#,
            ),
            (
                r#"{"context": "ftp://h/c.tar", "build_file": "Dockerfile"}"#,
                r#", "command": ["true"]"#,
            ),
            (r#""Dockerfile""#, r#", "command": ["true"]"#),
        ] {
            assert!(
                JobDescription::parse(&built(build, rest)).is_err(),
                "{build} {rest}"
            );
        }
        // An image build job needs a build definition.
        assert!(JobDescription::parse(
            r#"{"kind": "image_build", "image": "alpine", "timeout_seconds": 60}"#
        )
        .is_err());
        assert!(JobDescription::parse(
            r#"{"image": {"build": {}, "x": 1}, "command": ["true"], "timeout_seconds": 60}"#
        )
        .is_err());
    }

    #[test]
    fn filling_in_the_context_hash() {
        let mut doc = ci_json::parse(&built(
            r#"{"context": "stored:c.tar", "build_file": "Dockerfile"}"#,
            r#", "command": ["true"]"#,
        ))
        .unwrap();
        set_context_sha256(&mut doc, HASH);
        let job = JobDescription::parse(&doc.to_json()).unwrap();
        let Image::Build(build) = &job.image else {
            panic!("expected a build definition")
        };
        assert_eq!(build.context_sha256.as_deref(), Some(HASH));
        // Setting it again replaces it.
        set_context_sha256(&mut doc, &"1".repeat(64));
        let job = JobDescription::parse(&doc.to_json()).unwrap();
        let Image::Build(build) = &job.image else {
            panic!("expected a build definition")
        };
        assert_eq!(
            build.context_sha256.as_deref(),
            Some("1".repeat(64).as_str())
        );
        // A description with an image reference is left alone.
        let mut plain = ci_json::parse(MINIMAL).unwrap();
        let before = plain.clone();
        set_context_sha256(&mut plain, HASH);
        assert_eq!(plain, before);
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
