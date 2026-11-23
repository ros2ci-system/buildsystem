// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! What the coordinator and its workers say to each other.
//!
//! Workers make plain HTTPS requests with JSON bodies, and the coordinator
//! never starts a conversation. There are four calls:
//!
//! | Call | When | Reply |
//! |---|---|---|
//! | Register | Once at start-up | A worker id and a session token |
//! | Claim | Every few seconds while idle | A job, or nothing |
//! | Report | Every few seconds while a job runs | Carry on, cancel or abandon |
//! | Finish | Once per job | An acknowledgement |
//!
//! A claimed job belongs to its worker only while Reports keep arriving.
//! Each claim gets a new attempt number, and every later call carries it,
//! so a worker that lost its lease cannot finish a job that another
//! worker now holds.

pub mod job;

pub use job::{BuildDefinition, Image, Input, JobDescription, JobKind, Source};

/// Sent with Register. The coordinator refuses versions it does not know.
pub const PROTOCOL_VERSION: i64 = 1;

/// The paths of the worker API.
pub const PATH_REGISTER: &str = "/worker/v1/register";
pub const PATH_CLAIM: &str = "/worker/v1/claim";
pub const PATH_REPORT: &str = "/worker/v1/report";
pub const PATH_FINISH: &str = "/worker/v1/finish";

/// Where a worker uploads one artifact of one attempt.
pub fn artifact_path(job_id: i64, attempt: i64, relative: &str) -> String {
    format!("/worker/v1/jobs/{job_id}/attempts/{attempt}/artifacts/{relative}")
}

/// Where a worker downloads a stored file that its job reads.
pub fn stored_file_path(job_id: i64, attempt: i64, name: &str) -> String {
    format!("/worker/v1/jobs/{job_id}/attempts/{attempt}/files/{name}")
}

/// How a job ended, as the worker reports it in Finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// The job ran and failed.
    BuildFailure,
    /// The job ran past its timeout and was killed.
    Timeout,
    /// The job was stopped because the coordinator asked.
    Cancelled,
    /// The job could not be run, or said its failure was not its own.
    InfrastructureFailure,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::BuildFailure => "build_failure",
            Outcome::Timeout => "timeout",
            Outcome::Cancelled => "cancelled",
            Outcome::InfrastructureFailure => "infrastructure_failure",
        }
    }

    pub fn parse(text: &str) -> Option<Outcome> {
        Some(match text {
            "success" => Outcome::Success,
            "build_failure" => Outcome::BuildFailure,
            "timeout" => Outcome::Timeout,
            "cancelled" => Outcome::Cancelled,
            "infrastructure_failure" => Outcome::InfrastructureFailure,
            _ => return None,
        })
    }
}

/// What the coordinator tells a worker in reply to a Report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Keep running the job.
    Continue,
    /// Stop the job and Finish it as cancelled.
    Cancel,
    /// The job no longer belongs to this worker. Stop it and report
    /// nothing more about it.
    Abandon,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Continue => "continue",
            Action::Cancel => "cancel",
            Action::Abandon => "abandon",
        }
    }

    pub fn parse(text: &str) -> Option<Action> {
        Some(match text {
            "continue" => Action::Continue,
            "cancel" => Action::Cancel,
            "abandon" => Action::Abandon,
            _ => return None,
        })
    }
}

/// Whether a relative artifact path is safe to store and serve: plain
/// names separated by `/`, with nothing that means something special on
/// Linux or Windows.
pub fn is_safe_artifact_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment.len() <= 255
                && segment != "."
                && segment != ".."
                && !segment.ends_with(['.', ' '])
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-+@=, ".contains(c))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_and_actions_round_trip() {
        for outcome in [
            Outcome::Success,
            Outcome::BuildFailure,
            Outcome::Timeout,
            Outcome::Cancelled,
            Outcome::InfrastructureFailure,
        ] {
            assert_eq!(Outcome::parse(outcome.as_str()), Some(outcome));
        }
        for action in [Action::Continue, Action::Cancel, Action::Abandon] {
            assert_eq!(Action::parse(action.as_str()), Some(action));
        }
        assert_eq!(Outcome::parse("nonsense"), None);
        assert_eq!(Action::parse(""), None);
    }

    #[test]
    fn artifact_paths() {
        for good in ["results.json", "sub/dir/file.txt", "a b/c=d,e@f+g.tar.gz"] {
            assert!(is_safe_artifact_path(good), "{good}");
        }
        for bad in [
            "",
            "/abs",
            "a//b",
            "a/",
            "..",
            "a/../b",
            "./a",
            "a\\b",
            "C:/x",
            "a:b",
            "trailing.",
            "trailing ",
            "nul\0",
            "é",
            "a?b",
            "a%2fb",
        ] {
            assert!(!is_safe_artifact_path(bad), "{bad:?}");
        }
        assert_eq!(
            artifact_path(7, 2, "out/x.txt"),
            "/worker/v1/jobs/7/attempts/2/artifacts/out/x.txt"
        );
    }
}
