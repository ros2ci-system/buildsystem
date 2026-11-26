// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Asks a git server which commit a branch is at.
//!
//! This is all the coordinator does with git: it lists refs over HTTP,
//! which is the first step of a fetch, and reads no repository contents.

use ci_http::Request;
use ci_net::{Connector, Url};

/// Largest ref listing read. A repository with very many refs is not
/// what this is for.
const MAX_LISTING: usize = 32 * 1024 * 1024;

/// The commit a branch of a repository names. `repository` is the
/// repository's https address, as given to `git clone`.
pub fn branch_commit(
    connector: &Connector,
    repository: &str,
    branch: &str,
) -> Result<String, String> {
    let address = format!(
        "{}/info/refs?service=git-upload-pack",
        repository.trim_end_matches('/')
    );
    let url = Url::parse(&address).map_err(|e| format!("{repository}: {e}"))?;
    let host = url.host_header();
    // Git servers answer the smart protocol to a client that says it is git.
    let request = Request::new("GET", &url.target, &host)
        .header("User-Agent", "git/2.0")
        .header("Accept", "*/*");
    let mut response = connector
        .send(&url, request)
        .map_err(|e| format!("{repository}: {e}"))?;
    if response.status != 200 {
        return Err(format!(
            "{repository}: the server answered {} to a request for its refs",
            response.status
        ));
    }
    let body = response
        .body_bytes(MAX_LISTING)
        .map_err(|e| format!("{repository}: {e}"))?;
    let wanted = format!("refs/heads/{branch}");
    parse_refs(&body)
        .ok_or_else(|| format!("{repository}: the server's list of refs could not be read"))?
        .into_iter()
        .find(|(_, name)| *name == wanted)
        .map(|(commit, _)| commit)
        .ok_or_else(|| format!("{repository} has no branch {branch}"))
}

/// Reads a ref advertisement: lines framed by a four-digit hex length,
/// each `<commit> <ref name>`, the first with a list of capabilities
/// after a zero byte.
fn parse_refs(mut body: &[u8]) -> Option<Vec<(String, String)>> {
    let mut refs = Vec::new();
    while !body.is_empty() {
        let length = usize::from_str_radix(std::str::from_utf8(body.get(..4)?).ok()?, 16).ok()?;
        if length == 0 {
            // A flush: the end of a section.
            body = &body[4..];
            continue;
        }
        if length < 4 || length > body.len() {
            return None;
        }
        let line = &body[4..length];
        body = &body[length..];
        let line = line.split(|&b| b == 0).next()?;
        let line = std::str::from_utf8(line).ok()?.trim_end_matches('\n');
        if line.starts_with('#') {
            continue;
        }
        let (commit, name) = line.split_once(' ')?;
        let hex = commit.len() >= 40 && commit.bytes().all(|b| b.is_ascii_hexdigit());
        if !hex {
            return None;
        }
        refs.push((commit.to_ascii_lowercase(), name.to_string()));
    }
    Some(refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(text: &str) -> String {
        format!("{:04x}{text}", text.len() + 4)
    }

    #[test]
    fn a_ref_advertisement_is_read() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let body = [
            packet("# service=git-upload-pack\n"),
            "0000".to_string(),
            packet(&format!("{a} HEAD\0multi_ack thin-pack\n")),
            packet(&format!("{a} refs/heads/main\n")),
            packet(&format!("{b} refs/heads/other\n")),
            "0000".to_string(),
        ]
        .concat();
        assert_eq!(
            parse_refs(body.as_bytes()).unwrap(),
            vec![
                (a.clone(), "HEAD".to_string()),
                (a, "refs/heads/main".to_string()),
                (b, "refs/heads/other".to_string()),
            ]
        );
    }

    #[test]
    fn a_damaged_advertisement_is_refused() {
        assert!(parse_refs(b"zzzz").is_none());
        assert!(parse_refs(b"0010short").is_none());
        assert!(parse_refs(packet("not-a-commit refs/heads/main\n").as_bytes()).is_none());
        assert_eq!(parse_refs(b"").unwrap(), vec![]);
    }
}
