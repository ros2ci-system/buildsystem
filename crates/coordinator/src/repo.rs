// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Package repositories: what may go into one, and how its index is
//! written and signed.
//!
//! A build job describes the packages it built in `repo-metadata.json`,
//! using the distribution's own tools: the control fields of each Debian
//! package, or the entries `createrepo_c` writes for each RPM. Nothing in
//! that description is trusted. This module checks it against what the
//! build was asked to produce and against the files that were uploaded,
//! and writes every name, size, hash and path in the index itself. The
//! job's text is copied only as field values, escaped.
//!
//! The index is plain files: `Packages`, `Sources` and `Release` for
//! apt; `primary.xml`, `filelists.xml`, `other.xml` and `repomd.xml` for
//! RPM. None are compressed, because there is no compressor here, and
//! both apt and dnf read them as they are. Only the top of each index is
//! signed, as in the ROS repositories: the files under it are named by
//! hash.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use ci_crypto::{hex_encode, sha256};
use ci_json::Value;
use ci_openpgp::SigningKey;

use crate::db::Artifact;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Apt,
    Rpm,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Apt => "apt",
            Format::Rpm => "rpm",
        }
    }

    pub fn parse(text: &str) -> Option<Format> {
        match text {
            "apt" => Some(Format::Apt),
            "rpm" => Some(Format::Rpm),
            _ => None,
        }
    }

    /// The `format` a job's description must state.
    fn package_format(self) -> &'static str {
        match self {
            Format::Apt => "deb",
            Format::Rpm => "rpm",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Repository {
    pub name: String,
    pub format: Format,
    /// apt only: the suite, such as `resolute`.
    pub suite: String,
    /// What follows the rebuild counter in a package's version: the
    /// release code name for apt, the dist tag for RPM.
    pub version_suffix: String,
    /// The architectures built for, as jobs name them (`amd64`).
    pub architectures: Vec<String>,
    /// The architecture whose builds supply the source packages.
    pub source_arch: String,
}

/// The name a package format gives an architecture that jobs call
/// `amd64` or `arm64`.
pub fn format_arch(format: Format, arch: &str) -> Option<&'static str> {
    match (format, arch) {
        (Format::Apt, "amd64") => Some("amd64"),
        (Format::Apt, "arm64") => Some("arm64"),
        (Format::Rpm, "amd64") => Some("x86_64"),
        (Format::Rpm, "arm64") => Some("aarch64"),
        _ => None,
    }
}

/// One build to publish: what was asked for, and what was uploaded.
pub struct Build<'a> {
    pub repository: &'a Repository,
    /// The ROS package name. It names the directory the files go in.
    pub package: &'a str,
    /// The architecture the job ran on, as jobs name it.
    pub arch: &'a str,
    /// The binary and source package names this build may publish.
    pub names: &'a [String],
    /// The version every package must carry, rebuild counter included.
    pub version: &'a str,
    pub artifacts: &'a [Artifact],
}

/// A file to put in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    /// Where the job uploaded it.
    pub artifact: String,
    /// Where it goes, relative to the repository's root.
    pub path: String,
    /// Which index lists it; empty for a file that only another entry
    /// refers to, such as a source tarball.
    pub section: String,
    pub size: i64,
    pub sha256: String,
    /// Its entry in the index, ready to be joined with the others.
    pub metadata: String,
}

/// A file already in the repository, as the index needs it.
#[derive(Debug, Clone)]
pub struct Stored {
    pub path: String,
    pub section: String,
    pub metadata: String,
}

/// Checks a job's description of what it built and decides where each
/// file goes. An error is a sentence for whoever asked for the publish.
pub fn plan(build: &Build<'_>, metadata: &Value) -> Result<Vec<Planned>, String> {
    if metadata.get("version").and_then(Value::as_i64) != Some(1) {
        return Err("repo-metadata.json is not version 1".to_string());
    }
    let format = build.repository.format;
    let stated = metadata.get("format").and_then(Value::as_str).unwrap_or("");
    if stated != format.package_format() {
        return Err(format!(
            "the job built {stated:?} packages, and this repository holds {}",
            format.package_format()
        ));
    }
    let arch = format_arch(format, build.arch).ok_or_else(|| {
        format!(
            "no {} name for architecture {}",
            format.as_str(),
            build.arch
        )
    })?;
    let entries = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or("repo-metadata.json has no \"packages\" list")?;
    let mut planned = Vec::new();
    for entry in entries {
        let file = entry
            .get("file")
            .and_then(Value::as_str)
            .ok_or("a package entry has no \"file\"")?;
        let artifact = find_artifact(build.artifacts, file)?;
        match format {
            Format::Apt => plan_deb(build, arch, entry, artifact, &mut planned)?,
            Format::Rpm => plan_rpm(build, arch, entry, artifact, &mut planned)?,
        }
    }
    if !planned.iter().any(|p| !p.section.is_empty()) {
        return Err("the job described no packages to publish".to_string());
    }
    let mut seen = HashSet::new();
    for file in &planned {
        if !seen.insert(file.path.as_str()) {
            return Err(format!("two files would be published as {}", file.path));
        }
    }
    Ok(planned)
}

fn find_artifact<'a>(artifacts: &'a [Artifact], path: &str) -> Result<&'a Artifact, String> {
    artifacts
        .iter()
        .find(|a| a.path == path)
        .ok_or_else(|| format!("{path:?} is described but was not uploaded"))
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn check_name(build: &Build<'_>, name: &str) -> Result<(), String> {
    if build.names.iter().any(|n| n == name) {
        Ok(())
    } else {
        Err(format!(
            "the job built a package named {name:?}, which this build may not publish"
        ))
    }
}

fn check_version(build: &Build<'_>, name: &str, version: &str) -> Result<(), String> {
    if version == build.version {
        Ok(())
    } else {
        Err(format!(
            "{name} has version {version:?}, not the expected {:?}",
            build.version
        ))
    }
}

// ---------------------------------------------------------------------
// Debian packages
// ---------------------------------------------------------------------

/// Fields the index states itself, whatever the job says.
const DEB_OWN_FIELDS: &[&str] = &[
    "filename",
    "size",
    "md5sum",
    "sha1",
    "sha256",
    "sha512",
    "directory",
    "files",
    "checksums-sha1",
    "checksums-sha256",
    "checksums-sha512",
];

/// Reads `"fields": [[name, value], ...]` and checks that each pair can
/// be written as a control field without changing the structure of the
/// file around it. A value's continuation lines carry their leading
/// blank, as in the control file.
fn deb_fields(entry: &Value) -> Result<Vec<(&str, &str)>, String> {
    let list = entry
        .get("fields")
        .and_then(Value::as_array)
        .ok_or("a package entry has no \"fields\" list")?;
    let mut fields: Vec<(&str, &str)> = Vec::new();
    for pair in list {
        let (name, value) = match pair.as_array() {
            Some([name, value]) => (
                name.as_str().ok_or("a field name is not a string")?,
                value.as_str().ok_or("a field value is not a string")?,
            ),
            _ => return Err("a field is not a [name, value] pair".to_string()),
        };
        let name_ok = !name.is_empty()
            && name.len() <= 64
            && !name.starts_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !name_ok {
            return Err(format!("{name:?} is not a control field name"));
        }
        if fields.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            return Err(format!("the field {name} is given twice"));
        }
        let mut lines = value.split('\n');
        let first = lines.next().unwrap_or("");
        let value_ok = !value
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
            && first.trim() == first
            && lines.all(|line| line.starts_with([' ', '\t']) && !line.trim().is_empty());
        if !value_ok {
            return Err(format!("the value of field {name} is malformed"));
        }
        fields.push((name, value));
    }
    Ok(fields)
}

fn deb_field<'a>(fields: &[(&str, &'a str)], name: &str) -> Result<&'a str, String> {
    fields
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, value)| *value)
        .ok_or_else(|| format!("a package entry has no {name} field"))
}

/// Writes one field. A value that starts on the next line leaves no
/// blank after the colon.
fn push_deb_field(stanza: &mut String, name: &str, value: &str) {
    stanza.push_str(name);
    stanza.push(':');
    if !value.starts_with('\n') {
        stanza.push(' ');
    }
    stanza.push_str(value);
    stanza.push('\n');
}

fn plan_deb(
    build: &Build<'_>,
    arch: &str,
    entry: &Value,
    artifact: &Artifact,
    planned: &mut Vec<Planned>,
) -> Result<(), String> {
    let fields = deb_fields(entry)?;
    let mut stanza = String::new();
    match entry.get("kind").and_then(Value::as_str) {
        Some("binary") => {
            let name = deb_field(&fields, "Package")?;
            check_name(build, name)?;
            check_version(build, name, deb_field(&fields, "Version")?)?;
            let architecture = deb_field(&fields, "Architecture")?;
            if architecture != arch && architecture != "all" {
                return Err(format!(
                    "{name} is for architecture {architecture:?}, and the job ran on {arch}"
                ));
            }
            let path = format!(
                "pool/{}/{}/{}",
                build.arch,
                build.package,
                base_name(&artifact.path)
            );
            for (field, value) in &fields {
                if !DEB_OWN_FIELDS.contains(&field.to_ascii_lowercase().as_str()) {
                    push_deb_field(&mut stanza, field, value);
                }
            }
            stanza.push_str(&format!(
                "Filename: {path}\nSize: {}\nSHA256: {}\n",
                artifact.size, artifact.sha256
            ));
            planned.push(Planned {
                artifact: artifact.path.clone(),
                path,
                section: format!("binary-{}", build.arch),
                size: artifact.size,
                sha256: artifact.sha256.clone(),
                metadata: stanza,
            });
        }
        Some("source") => {
            // Every architecture's job builds the source package, and the
            // results differ in their timestamps. One architecture's is
            // published.
            if build.arch != build.repository.source_arch {
                return Ok(());
            }
            let name = deb_field(&fields, "Source")?;
            check_name(build, name)?;
            check_version(build, name, deb_field(&fields, "Version")?)?;
            let directory = format!("pool/source/{}", build.package);
            let mut checksums = format!(
                "\n {} {} {}",
                artifact.sha256,
                artifact.size,
                base_name(&artifact.path)
            );
            // The files the .dsc names must be the ones uploaded beside
            // it, with the sizes and hashes it states.
            let beside = artifact
                .path
                .rsplit_once('/')
                .map_or(String::new(), |(dir, _)| format!("{dir}/"));
            for line in deb_field(&fields, "Checksums-Sha256")?.lines().skip(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                let [hash, size, file] = parts.as_slice() else {
                    return Err("a Checksums-Sha256 line is malformed".to_string());
                };
                let part = find_artifact(build.artifacts, &format!("{beside}{file}"))?;
                if !part.sha256.eq_ignore_ascii_case(hash) || part.size.to_string() != *size {
                    return Err(format!(
                        "{file} is not the file the source package describes"
                    ));
                }
                checksums.push_str(&format!("\n {} {} {file}", part.sha256, part.size));
                planned.push(Planned {
                    artifact: part.path.clone(),
                    path: format!("{directory}/{file}"),
                    section: String::new(),
                    size: part.size,
                    sha256: part.sha256.clone(),
                    metadata: String::new(),
                });
            }
            stanza.push_str(&format!("Package: {name}\n"));
            for (field, value) in &fields {
                let lower = field.to_ascii_lowercase();
                if lower != "source" && !DEB_OWN_FIELDS.contains(&lower.as_str()) {
                    push_deb_field(&mut stanza, field, value);
                }
            }
            stanza.push_str(&format!(
                "Directory: {directory}\nChecksums-Sha256:{checksums}\n"
            ));
            planned.push(Planned {
                artifact: artifact.path.clone(),
                path: format!("{directory}/{}", base_name(&artifact.path)),
                section: "source".to_string(),
                size: artifact.size,
                sha256: artifact.sha256.clone(),
                metadata: stanza,
            });
        }
        _ => return Err("a package entry is neither \"binary\" nor \"source\"".to_string()),
    }
    Ok(())
}

// ---------------------------------------------------------------------
// RPM packages
// ---------------------------------------------------------------------

// An entry of createrepo_c's XML reaches here as a tree of JSON objects:
// {"tag": ..., "attrs": [[name, value], ...], "text": ..., "children": [...]}.
// The XML in the index is written from that tree, never copied, so a
// job cannot put markup of its own into the index.

const MAX_XML_DEPTH: usize = 16;

fn xml_tag(node: &Value) -> &str {
    node.get("tag").and_then(Value::as_str).unwrap_or("")
}

fn xml_attr<'a>(node: &'a Value, name: &str) -> Option<&'a str> {
    node.get("attrs")?
        .as_array()?
        .iter()
        .filter_map(Value::as_array)
        .find(|pair| pair.first().and_then(Value::as_str) == Some(name))
        .and_then(|pair| pair.get(1))
        .and_then(Value::as_str)
}

fn xml_child<'a>(node: &'a Value, tag: &str) -> Option<&'a Value> {
    node.get("children")?
        .as_array()?
        .iter()
        .find(|child| xml_tag(child) == tag)
}

fn xml_text(node: &Value) -> &str {
    node.get("text").and_then(Value::as_str).unwrap_or("")
}

fn is_xml_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || "_-:".contains(c))
}

fn xml_escape(text: &str, out: &mut String) -> Result<(), String> {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' => out.push(c),
            // XML 1.0 has no way to write the other control characters.
            c if c.is_control() || c == '\u{fffe}' || c == '\u{ffff}' => {
                return Err("the package description holds a control character".to_string())
            }
            c => out.push(c),
        }
    }
    Ok(())
}

/// Writes a tree as XML. With `location`, the top-level `location`
/// element is replaced by one that names that path.
fn xml_render(
    node: &Value,
    depth: usize,
    location: Option<&str>,
    out: &mut String,
) -> Result<(), String> {
    if depth > MAX_XML_DEPTH {
        return Err("a package description is nested too deeply".to_string());
    }
    let tag = xml_tag(node);
    if !is_xml_name(tag) {
        return Err(format!("{tag:?} is not an XML element name"));
    }
    out.push('<');
    out.push_str(tag);
    if let (1, "location", Some(path)) = (depth, tag, location) {
        out.push_str(" href=\"");
        xml_escape(path, out)?;
        out.push_str("\"/>");
        return Ok(());
    }
    for pair in node.get("attrs").and_then(Value::as_array).unwrap_or(&[]) {
        let (name, value) = match pair.as_array() {
            Some([name, value]) => (name.as_str(), value.as_str()),
            _ => (None, None),
        };
        let (Some(name), Some(value)) = (name, value) else {
            return Err("an XML attribute is not a [name, value] pair".to_string());
        };
        if !is_xml_name(name) || name.starts_with("xml") {
            return Err(format!("{name:?} is not an allowed XML attribute name"));
        }
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        xml_escape(value, out)?;
        out.push('"');
    }
    let children = node
        .get("children")
        .and_then(Value::as_array)
        .unwrap_or(&[]);
    let text = xml_text(node);
    if children.is_empty() && text.is_empty() {
        out.push_str("/>");
        return Ok(());
    }
    out.push('>');
    if children.is_empty() {
        xml_escape(text, out)?;
    } else {
        for child in children {
            xml_render(child, depth + 1, location, out)?;
        }
    }
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
    Ok(())
}

fn plan_rpm(
    build: &Build<'_>,
    arch: &str,
    entry: &Value,
    artifact: &Artifact,
    planned: &mut Vec<Planned>,
) -> Result<(), String> {
    let primary = entry
        .get("primary")
        .ok_or("a package entry has no \"primary\"")?;
    if xml_tag(primary) != "package" || xml_attr(primary, "type") != Some("rpm") {
        return Err("a package entry's \"primary\" is not an RPM entry".to_string());
    }
    let text_of = |tag: &str| xml_child(primary, tag).map(xml_text).unwrap_or("");
    let name = text_of("name");
    check_name(build, name)?;
    let package_arch = text_of("arch");
    if package_arch == "src" {
        if build.arch != build.repository.source_arch {
            return Ok(());
        }
    } else if package_arch != arch && package_arch != "noarch" {
        return Err(format!(
            "{name} is for architecture {package_arch:?}, and the job ran on {arch}"
        ));
    }
    let version = xml_child(primary, "version").ok_or("an RPM entry has no version")?;
    if xml_attr(version, "epoch") != Some("0") {
        return Err(format!("{name} has an epoch, which is not supported"));
    }
    check_version(
        build,
        name,
        &format!(
            "{}-{}",
            xml_attr(version, "ver").unwrap_or(""),
            xml_attr(version, "rel").unwrap_or("")
        ),
    )?;
    // The entry must describe the file that was uploaded.
    let checksum = xml_child(primary, "checksum").ok_or("an RPM entry has no checksum")?;
    let size = xml_child(primary, "size").and_then(|size| xml_attr(size, "package"));
    if xml_attr(checksum, "type") != Some("sha256")
        || !xml_text(checksum).eq_ignore_ascii_case(&artifact.sha256)
        || size != Some(artifact.size.to_string().as_str())
    {
        return Err(format!(
            "the entry for {name} does not describe the file that was uploaded"
        ));
    }
    let debug = name.ends_with("-debuginfo") || name.ends_with("-debugsource");
    let section = match (package_arch, debug) {
        ("src", _) => "SRPMS".to_string(),
        (_, true) => format!("{arch}/debug"),
        (_, false) => arch.to_string(),
    };
    let location = format!("Packages/{}", base_name(&artifact.path));

    let mut parts = Vec::new();
    let mut rendered = String::new();
    xml_render(primary, 0, Some(&location), &mut rendered)?;
    parts.push(("primary", Value::from(rendered)));
    for part in ["filelists", "other"] {
        let node = entry
            .get(part)
            .ok_or_else(|| format!("a package entry has no {part:?}"))?;
        if xml_tag(node) != "package"
            || xml_attr(node, "name") != Some(name)
            || xml_attr(node, "arch") != Some(package_arch)
            || !xml_attr(node, "pkgid").is_some_and(|id| id.eq_ignore_ascii_case(&artifact.sha256))
        {
            return Err(format!(
                "the {part} entry for {name} does not describe the same package"
            ));
        }
        let mut rendered = String::new();
        xml_render(node, 0, None, &mut rendered)?;
        parts.push((part, Value::from(rendered)));
    }
    planned.push(Planned {
        artifact: artifact.path.clone(),
        path: format!("{section}/{location}"),
        section,
        size: artifact.size,
        sha256: artifact.sha256.clone(),
        metadata: Value::object(parts).to_json(),
    });
    Ok(())
}

// ---------------------------------------------------------------------
// Writing the index
// ---------------------------------------------------------------------

/// Index files that nothing refers to any more are kept this long, for
/// clients that fetched the top of the index a moment before it changed.
const KEEP_OLD_INDEX: Duration = Duration::from_secs(3600);

/// Writes a file under a temporary name and renames it, so that nobody
/// reads half of it.
fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().expect("index paths have a parent");
    fs::create_dir_all(parent)?;
    let name = path.file_name().expect("index paths have a file name");
    let temporary = parent.join(format!(".new-{}", name.to_string_lossy()));
    let mut file = fs::File::create(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temporary, path)
}

/// Removes the files in `dir` that are not in `keep` and are old.
fn prune(dir: &Path, keep: &HashSet<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.contains(&name) || !entry.file_type()?.is_file() {
            continue;
        }
        let age = entry
            .metadata()?
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok());
        if age.is_some_and(|age| age > KEEP_OLD_INDEX) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// The entries of one section, in the order of their paths, so that the
/// same packages always give the same index.
fn section_entries<'a>(files: &'a [Stored], section: &str) -> Vec<&'a Stored> {
    let mut entries: Vec<&Stored> = files.iter().filter(|f| f.section == section).collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// Writes the whole index of a repository under `root` and signs it.
/// `files` is everything the repository holds. `now` is in seconds since
/// 1970.
pub fn write_index(
    root: &Path,
    repository: &Repository,
    files: &[Stored],
    key: &SigningKey,
    now: u64,
) -> io::Result<()> {
    write_atomic(&root.join("key.asc"), key.public_key_armored().as_bytes())?;
    match repository.format {
        Format::Apt => write_apt_index(root, repository, files, key, now),
        Format::Rpm => {
            let mut sections = vec!["SRPMS".to_string()];
            for arch in &repository.architectures {
                if let Some(arch) = format_arch(Format::Rpm, arch) {
                    sections.push(arch.to_string());
                    sections.push(format!("{arch}/debug"));
                }
            }
            for section in sections {
                write_rpm_index(
                    &root.join(&section),
                    &section_entries(files, &section),
                    key,
                    now,
                )?;
            }
            Ok(())
        }
    }
}

fn write_apt_index(
    root: &Path,
    repository: &Repository,
    files: &[Stored],
    key: &SigningKey,
    now: u64,
) -> io::Result<()> {
    let dist = root.join("dists").join(&repository.suite);
    let mut sections: Vec<(String, String)> = repository
        .architectures
        .iter()
        .map(|arch| (format!("binary-{arch}"), "Packages".to_string()))
        .collect();
    sections.push(("source".to_string(), "Sources".to_string()));

    let mut listing = String::new();
    for (section, file_name) in &sections {
        let contents = section_entries(files, section)
            .iter()
            .map(|entry| entry.metadata.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let hash = hex_encode(&sha256(contents.as_bytes()));
        let directory = dist.join("main").join(section);
        // The copy named by its hash is what apt fetches, so that an
        // index read during an update is never a mix of old and new.
        let by_hash = directory.join("by-hash").join("SHA256");
        write_atomic(&by_hash.join(&hash), contents.as_bytes())?;
        write_atomic(&directory.join(file_name), contents.as_bytes())?;
        prune(&by_hash, &HashSet::from([hash.clone()]))?;
        listing.push_str(&format!(
            " {hash} {} main/{section}/{file_name}\n",
            contents.len()
        ));
    }
    let release = format!(
        "Origin: {name}\nLabel: {name}\nSuite: {suite}\nCodename: {suite}\nDate: {date}\n\
         Architectures: {architectures}\nComponents: main\nAcquire-By-Hash: yes\nSHA256:\n{listing}",
        name = repository.name,
        suite = repository.suite,
        date = release_date(now),
        architectures = repository.architectures.join(" "),
    );
    let time = now as u32;
    let signed = key
        .sign_cleartext(&release, time)
        .map_err(io::Error::other)?;
    // InRelease last: it is the file apt reads first, and it stands alone.
    write_atomic(&dist.join("Release"), release.as_bytes())?;
    write_atomic(
        &dist.join("Release.gpg"),
        key.sign_detached(release.as_bytes(), time).as_bytes(),
    )?;
    write_atomic(&dist.join("InRelease"), signed.as_bytes())
}

/// The date as a `Release` file states it.
fn release_date(now: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = now / 86400;
    let seconds = now % 86400;
    // Days since 1970 to a calendar date, for the Gregorian calendar.
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} UTC",
        DAYS[(days % 7) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

fn write_rpm_index(
    directory: &Path,
    entries: &[&Stored],
    key: &SigningKey,
    now: u64,
) -> io::Result<()> {
    const PARTS: [(&str, &str, &str); 3] = [
        (
            "primary",
            "metadata",
            "xmlns=\"http://linux.duke.edu/metadata/common\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\"",
        ),
        (
            "filelists",
            "filelists",
            "xmlns=\"http://linux.duke.edu/metadata/filelists\"",
        ),
        (
            "other",
            "otherdata",
            "xmlns=\"http://linux.duke.edu/metadata/other\"",
        ),
    ];
    let mut parsed = Vec::new();
    for entry in entries {
        parsed.push(ci_json::parse(&entry.metadata).map_err(|_| {
            io::Error::other(format!("the stored entry of {} is damaged", entry.path))
        })?);
    }
    let repodata = directory.join("repodata");
    let mut keep = HashSet::from(["repomd.xml".to_string(), "repomd.xml.asc".to_string()]);
    let mut repomd = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <repomd xmlns=\"http://linux.duke.edu/metadata/repo\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\">\n\
         <revision>{now}</revision>\n"
    );
    for (part, root, namespaces) in PARTS {
        let mut contents = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{root} {namespaces} packages=\"{}\">\n",
            entries.len()
        );
        for entry in &parsed {
            contents.push_str(entry.get(part).and_then(Value::as_str).unwrap_or(""));
            contents.push('\n');
        }
        contents.push_str(&format!("</{root}>\n"));
        let hash = hex_encode(&sha256(contents.as_bytes()));
        let name = format!("{hash}-{part}.xml");
        write_atomic(&repodata.join(&name), contents.as_bytes())?;
        repomd.push_str(&format!(
            "<data type=\"{part}\">\n\
             <checksum type=\"sha256\">{hash}</checksum>\n\
             <location href=\"repodata/{name}\"/>\n\
             <timestamp>{now}</timestamp>\n\
             <size>{}</size>\n\
             </data>\n",
            contents.len()
        ));
        keep.insert(name);
    }
    repomd.push_str("</repomd>\n");
    // The signature first: between the two renames a client can see the
    // new signature with the old repomd.xml, and it then tries again.
    write_atomic(
        &repodata.join("repomd.xml.asc"),
        key.sign_detached(repomd.as_bytes(), now as u32).as_bytes(),
    )?;
    write_atomic(&repodata.join("repomd.xml"), repomd.as_bytes())?;
    prune(&repodata, &keep)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(format: Format) -> Repository {
        Repository {
            name: "test".to_string(),
            format,
            suite: if format == Format::Apt {
                "resolute"
            } else {
                ""
            }
            .to_string(),
            version_suffix: String::new(),
            architectures: vec!["amd64".to_string(), "arm64".to_string()],
            source_arch: "amd64".to_string(),
        }
    }

    fn artifact(path: &str, size: i64, fill: char) -> Artifact {
        Artifact {
            path: path.to_string(),
            size,
            sha256: fill.to_string().repeat(64),
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn deb_metadata(package: &str, version: &str) -> Value {
        let a = "a".repeat(64);
        let text = r#"{"version": 1, "format": "deb", "packages": [
            {"file": "packages/ros-rolling-my-tool_1.2.3-4resolute_amd64.deb", "kind": "binary",
             "fields": [["Package", "PACKAGE"], ["Version", "VERSION"], ["Architecture", "amd64"],
                        ["Depends", "python3"], ["Filename", "elsewhere.deb"], ["Size", "1"],
                        ["Description", "A tool\n It does nothing."]]},
            {"file": "packages/ros-rolling-my-tool_1.2.3-4resolute.dsc", "kind": "source",
             "fields": [["Format", "3.0 (quilt)"], ["Source", "ros-rolling-my-tool"],
                        ["Version", "1.2.3-4resolute"],
                        ["Checksums-Sha256", "\n AAAA 300 ros-rolling-my-tool_1.2.3.orig.tar.gz"],
                        ["Files", "\n 00 300 ros-rolling-my-tool_1.2.3.orig.tar.gz"]]}
        ]}"#
        .replace("PACKAGE", package)
        .replace("VERSION", version)
        .replace("AAAA", &a);
        ci_json::parse(&text).unwrap()
    }

    fn deb_artifacts() -> Vec<Artifact> {
        vec![
            artifact(
                "packages/ros-rolling-my-tool_1.2.3-4resolute_amd64.deb",
                100,
                'd',
            ),
            artifact("packages/ros-rolling-my-tool_1.2.3-4resolute.dsc", 200, 'c'),
            artifact("packages/ros-rolling-my-tool_1.2.3.orig.tar.gz", 300, 'a'),
            artifact("repo-metadata.json", 1, '0'),
        ]
    }

    fn deb_plan(arch: &str, package: &str, version: &str) -> Result<Vec<Planned>, String> {
        let repository = repository(Format::Apt);
        let artifacts = deb_artifacts();
        let names = names(&["ros-rolling-my-tool"]);
        plan(
            &Build {
                repository: &repository,
                package: "my_tool",
                arch,
                names: &names,
                version: "1.2.3-4resolute",
                artifacts: &artifacts,
            },
            &deb_metadata(package, version),
        )
    }

    #[test]
    fn deb_plan_states_its_own_paths_and_hashes() {
        let planned = deb_plan("amd64", "ros-rolling-my-tool", "1.2.3-4resolute").unwrap();
        assert_eq!(planned.len(), 3);
        let binary = &planned[0];
        assert_eq!(
            binary.path,
            "pool/amd64/my_tool/ros-rolling-my-tool_1.2.3-4resolute_amd64.deb"
        );
        assert_eq!(binary.section, "binary-amd64");
        assert_eq!(
            binary.metadata,
            format!(
                "Package: ros-rolling-my-tool\nVersion: 1.2.3-4resolute\nArchitecture: amd64\n\
                 Depends: python3\nDescription: A tool\n It does nothing.\n\
                 Filename: {}\nSize: 100\nSHA256: {}\n",
                binary.path,
                "d".repeat(64)
            )
        );
        assert_eq!(planned[1].section, "");
        assert_eq!(
            planned[1].path,
            "pool/source/my_tool/ros-rolling-my-tool_1.2.3.orig.tar.gz"
        );
        let source = &planned[2];
        assert_eq!(source.section, "source");
        assert_eq!(
            source.metadata,
            format!(
                "Package: ros-rolling-my-tool\nFormat: 3.0 (quilt)\nVersion: 1.2.3-4resolute\n\
                 Directory: pool/source/my_tool\nChecksums-Sha256:\n \
                 {} 200 ros-rolling-my-tool_1.2.3-4resolute.dsc\n \
                 {} 300 ros-rolling-my-tool_1.2.3.orig.tar.gz\n",
                "c".repeat(64),
                "a".repeat(64)
            )
        );
    }

    #[test]
    fn deb_plan_refuses_what_was_not_asked_for() {
        let error = deb_plan("amd64", "libc6", "1.2.3-4resolute").unwrap_err();
        assert!(error.contains("may not publish"), "{error}");
        let error = deb_plan("amd64", "ros-rolling-my-tool", "1.2.3-3resolute").unwrap_err();
        assert!(error.contains("not the expected"), "{error}");
        // A job on another architecture cannot publish this one's package.
        let error = deb_plan("arm64", "ros-rolling-my-tool", "1.2.3-4resolute").unwrap_err();
        assert!(error.contains("architecture"), "{error}");
    }

    #[test]
    fn deb_fields_cannot_break_out_of_their_stanza() {
        for (name, value) in [
            ("Depends", "a\n\nPackage: evil"),
            ("Depends", "a\nPackage: evil"),
            ("Depends", "a\n "),
            ("Depends", "a\r"),
            ("Bad Name", "a"),
            ("Bad:Name", "a"),
        ] {
            let entry =
                Value::object([("fields", Value::Array(vec![Value::strings([name, value])]))]);
            assert!(deb_fields(&entry).is_err(), "{name:?} {value:?}");
        }
        let twice = Value::object([(
            "fields",
            Value::Array(vec![
                Value::strings(["Package", "a"]),
                Value::strings(["package", "b"]),
            ]),
        )]);
        assert!(deb_fields(&twice).is_err());
    }

    fn rpm_metadata(name: &str, arch: &str, release: &str) -> Value {
        let text = r#"{"version": 1, "format": "rpm", "packages": [
          {"file": "packages/FILE.rpm",
           "primary": {"tag": "package", "attrs": [["type", "rpm"]], "children": [
              {"tag": "name", "text": "NAME"},
              {"tag": "arch", "text": "ARCH"},
              {"tag": "version", "attrs": [["epoch", "0"], ["ver", "1.2.3"], ["rel", "RELEASE"]]},
              {"tag": "checksum", "attrs": [["type", "sha256"], ["pkgid", "YES"]], "text": "HASH"},
              {"tag": "summary", "text": "Tools & <things>"},
              {"tag": "size", "attrs": [["package", "100"], ["installed", "5"]]},
              {"tag": "location", "attrs": [["href", "../../etc/passwd"], ["xml:base", "http://evil"]]},
              {"tag": "format", "children": [
                 {"tag": "rpm:provides", "children": [
                    {"tag": "rpm:entry", "attrs": [["name", "ros-rolling(my_tool)"]]}]}]}]},
           "filelists": {"tag": "package", "attrs": [["pkgid", "HASH"], ["name", "NAME"], ["arch", "ARCH"]],
              "children": [{"tag": "file", "text": "/opt/ros/rolling/a\"b"}]},
           "other": {"tag": "package", "attrs": [["pkgid", "HASH"], ["name", "NAME"], ["arch", "ARCH"]]}}
        ]}"#
        .replace("NAME", name)
        .replace("ARCH", arch)
        .replace("RELEASE", release)
        .replace("HASH", &"d".repeat(64))
        .replace("FILE", &format!("{name}-1.2.3-{release}.{arch}"));
        ci_json::parse(&text).unwrap()
    }

    fn rpm_plan(name: &str, arch: &str, job_arch: &str) -> Result<Vec<Planned>, String> {
        let repository = repository(Format::Rpm);
        let artifacts = vec![artifact(
            &format!("packages/{name}-1.2.3-4.el10.{arch}.rpm"),
            100,
            'd',
        )];
        let names = names(&[
            "ros-rolling-my-tool",
            "ros-rolling-my-tool-runtime",
            "ros-rolling-my-tool-debugsource",
        ]);
        plan(
            &Build {
                repository: &repository,
                package: "my_tool",
                arch: job_arch,
                names: &names,
                version: "1.2.3-4.el10",
                artifacts: &artifacts,
            },
            &rpm_metadata(name, arch, "4.el10"),
        )
    }

    #[test]
    fn rpm_plan_writes_the_xml_itself() {
        let planned = rpm_plan("ros-rolling-my-tool-runtime", "x86_64", "amd64").unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].section, "x86_64");
        assert_eq!(
            planned[0].path,
            "x86_64/Packages/ros-rolling-my-tool-runtime-1.2.3-4.el10.x86_64.rpm"
        );
        let parts = ci_json::parse(&planned[0].metadata).unwrap();
        let primary = parts.get("primary").unwrap().as_str().unwrap();
        assert!(
            primary.starts_with("<package type=\"rpm\"><name>ros-rolling-my-tool-runtime</name>")
        );
        assert!(primary.contains("<summary>Tools &amp; &lt;things&gt;</summary>"));
        assert!(primary.contains(
            "<location href=\"Packages/ros-rolling-my-tool-runtime-1.2.3-4.el10.x86_64.rpm\"/>"
        ));
        assert!(!primary.contains("evil") && !primary.contains("passwd"));
        assert!(primary.contains("<rpm:entry name=\"ros-rolling(my_tool)\"/>"));
        let filelists = parts.get("filelists").unwrap().as_str().unwrap();
        assert!(filelists.ends_with("<file>/opt/ros/rolling/a&quot;b</file></package>"));
    }

    #[test]
    fn rpm_plan_sorts_packages_into_sections() {
        let debug = rpm_plan("ros-rolling-my-tool-debugsource", "x86_64", "amd64").unwrap();
        assert_eq!(debug[0].section, "x86_64/debug");
        let source = rpm_plan("ros-rolling-my-tool", "src", "amd64").unwrap();
        assert_eq!(source[0].section, "SRPMS");
        // Only one architecture's job supplies the source package.
        let error = rpm_plan("ros-rolling-my-tool", "src", "arm64").unwrap_err();
        assert!(error.contains("no packages"), "{error}");
    }

    #[test]
    fn rpm_plan_refuses_what_was_not_asked_for() {
        let error = rpm_plan("glibc", "x86_64", "amd64").unwrap_err();
        assert!(error.contains("may not publish"), "{error}");
        let error = rpm_plan("ros-rolling-my-tool-runtime", "x86_64", "arm64").unwrap_err();
        assert!(error.contains("architecture"), "{error}");
    }

    #[test]
    fn xml_names_and_text_are_checked() {
        let bad_tag = ci_json::parse(r#"{"tag": "a><b"}"#).unwrap();
        assert!(xml_render(&bad_tag, 0, None, &mut String::new()).is_err());
        let bad_attr = ci_json::parse(r#"{"tag": "a", "attrs": [["x y", "1"]]}"#).unwrap();
        assert!(xml_render(&bad_attr, 0, None, &mut String::new()).is_err());
        let control = ci_json::parse(r#"{"tag": "a", "text": "x\u0001"}"#).unwrap();
        assert!(xml_render(&control, 0, None, &mut String::new()).is_err());
    }

    #[test]
    fn release_dates() {
        assert_eq!(release_date(0), "Thu, 01 Jan 1970 00:00:00 UTC");
        assert_eq!(release_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 UTC");
        assert_eq!(
            release_date(1_795_651_200 + 3723),
            "Thu, 26 Nov 2026 01:02:03 UTC"
        );
    }

    #[test]
    fn written_indexes_name_their_parts_by_hash() {
        let root = std::env::temp_dir().join(format!("ros2ci-repo-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let key = SigningKey::generate("Test <test@example.invalid>", 1).unwrap();

        let apt = root.join("apt");
        let planned = deb_plan("amd64", "ros-rolling-my-tool", "1.2.3-4resolute").unwrap();
        let files: Vec<Stored> = planned
            .iter()
            .map(|p| Stored {
                path: p.path.clone(),
                section: p.section.clone(),
                metadata: p.metadata.clone(),
            })
            .collect();
        write_index(&apt, &repository(Format::Apt), &files, &key, 1_795_694_400).unwrap();
        let dist = apt.join("dists/resolute");
        let release = fs::read_to_string(dist.join("Release")).unwrap();
        let packages = fs::read(dist.join("main/binary-amd64/Packages")).unwrap();
        let hash = hex_encode(&sha256(&packages));
        assert!(release.contains(&format!(
            " {hash} {} main/binary-amd64/Packages\n",
            packages.len()
        )));
        assert_eq!(
            fs::read(dist.join("main/binary-amd64/by-hash/SHA256").join(&hash)).unwrap(),
            packages
        );
        assert_eq!(
            fs::read(dist.join("main/binary-arm64/Packages")).unwrap(),
            b""
        );
        assert!(fs::read_to_string(dist.join("InRelease"))
            .unwrap()
            .contains(&release));
        assert!(dist.join("Release.gpg").exists() && apt.join("key.asc").exists());

        let rpm = root.join("rpm");
        let planned = rpm_plan("ros-rolling-my-tool-runtime", "x86_64", "amd64").unwrap();
        let files = vec![Stored {
            path: planned[0].path.clone(),
            section: planned[0].section.clone(),
            metadata: planned[0].metadata.clone(),
        }];
        write_index(&rpm, &repository(Format::Rpm), &files, &key, 1_795_694_400).unwrap();
        let repomd = fs::read_to_string(rpm.join("x86_64/repodata/repomd.xml")).unwrap();
        let start = repomd.find("repodata/").unwrap();
        let end = start + repomd[start..].find('"').unwrap();
        let primary = fs::read_to_string(rpm.join("x86_64").join(&repomd[start..end])).unwrap();
        assert!(primary.contains("packages=\"1\""));
        assert!(repomd.contains(&hex_encode(&sha256(primary.as_bytes()))));
        for empty in ["SRPMS", "x86_64/debug", "aarch64"] {
            assert!(rpm.join(empty).join("repodata/repomd.xml.asc").exists());
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
