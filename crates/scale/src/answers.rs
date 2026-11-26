// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! What the jobs of a release would leave behind, made up on the spot.
//!
//! Each answer has the form the coordinator expects of the real job: a
//! release check's `results.json`, a source archive's tarball and
//! record, and a package build's packages with their description. The
//! packages are a few kilobytes of filler; their index entries are about
//! the size of real ones, because the size of the index is part of what
//! the scale test measures.

use ci_crypto::{hex_encode, sha256};
use ci_json::Value;

use crate::distribution::{Distribution, Package};

/// The files a job uploads, by artifact path.
pub type Files = Vec<(String, Vec<u8>)>;

fn word<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("")
}

fn list<'a>(value: &'a Value, name: &str) -> &'a [Value] {
    value
        .get(name)
        .and_then(Value::as_array)
        .unwrap_or_default()
}

/// Filler that differs for every label.
fn filler(label: &str, size: usize) -> Vec<u8> {
    let mut block = sha256(label.as_bytes());
    let mut out = Vec::with_capacity(size + block.len());
    while out.len() < size {
        out.extend_from_slice(&block);
        block = sha256(&block);
    }
    out.truncate(size);
    out
}

fn hash(data: &[u8]) -> String {
    hex_encode(&sha256(data))
}

fn source_commit(repository: &str, version: &str) -> String {
    hash(format!("{repository} {version}").as_bytes())[..40].to_string()
}

fn package_records(packages: &[Package], version: &str) -> Value {
    Value::Array(
        packages
            .iter()
            .map(|package| {
                // A key the platform provides, as every real package has.
                let depends = package.depends.iter().map(String::as_str).chain(["cmake"]);
                Value::object([
                    ("name", Value::from(package.name.as_str())),
                    ("path", Value::from(package.name.as_str())),
                    ("version", Value::from(version)),
                    ("build_type", Value::from("ament_cmake")),
                    ("depends", Value::strings(depends)),
                    ("test_depends", Value::strings(&package.test_depends)),
                    ("groups", Value::Array(Vec::new())),
                ])
            })
            .collect(),
    )
}

/// The release check: every release of the distribution, each new unless
/// the archive already holds it.
pub fn release_check(distribution: &Distribution, parameters: &Value) -> Files {
    let archived: Vec<(&str, &str)> = list(parameters, "archived")
        .iter()
        .map(|record| (word(record, "repository"), word(record, "version")))
        .collect();
    let releases = distribution
        .repositories
        .iter()
        .map(|repository| {
            let known = archived.contains(&(repository.name.as_str(), repository.version.as_str()));
            Value::object([
                ("repository", Value::from(repository.name.as_str())),
                (
                    "url",
                    Value::from(format!("https://scale.invalid/{}.git", repository.name)),
                ),
                ("version", Value::from(repository.version.as_str())),
                ("tag", Value::from(repository.version.as_str())),
                (
                    "commit",
                    Value::from(source_commit(&repository.name, &repository.version)),
                ),
                ("state", Value::from(if known { "archived" } else { "new" })),
                ("skipped", Value::Array(Vec::new())),
                (
                    "packages",
                    package_records(&repository.packages, &repository.version),
                ),
            ])
        })
        .collect();
    let results = Value::object([
        ("status", Value::from("success")),
        ("problems", Value::Array(Vec::new())),
        ("moved_tags", Value::Array(Vec::new())),
        ("releases", Value::Array(releases)),
    ]);
    vec![("results.json".to_string(), results.to_json().into_bytes())]
}

/// The source archive job for one release.
pub fn source_archive(distribution: &Distribution, parameters: &Value) -> Result<Files, String> {
    let name = word(parameters, "repository");
    let version = word(parameters, "version");
    let repository = distribution
        .repositories
        .iter()
        .find(|repository| repository.name == name)
        .ok_or_else(|| format!("the distribution has no repository {name}"))?;
    let tarball = filler(&format!("source of {name} {version}"), 8 * 1024);
    let mut record = vec![("sha256", Value::from(hash(&tarball)))];
    for field in [
        "repository",
        "url",
        "version",
        "tag",
        "commit",
        "rosdistro_url",
    ] {
        record.push((field, Value::from(word(parameters, field))));
    }
    record.push(("skipped", Value::Array(Vec::new())));
    record.push(("packages", package_records(&repository.packages, version)));
    Ok(vec![
        ("source.tar.gz".to_string(), tarball),
        (
            "record.json".to_string(),
            Value::object(record).to_json().into_bytes(),
        ),
    ])
}

/// What a build is for.
struct Build<'a> {
    package: &'a Package,
    /// The binary package's name.
    name: String,
    version: &'a str,
    revision: i64,
    suffix: &'a str,
    prefix: &'a str,
}

impl Build<'_> {
    fn binary_name(&self, package: &str) -> String {
        format!("{}{}", self.prefix, package.replace('_', "-"))
    }

    fn filler(&self, file: &str) -> Vec<u8> {
        filler(&format!("{file} {}", self.revision), 2 * 1024)
    }
}

/// A package build: binary, debug and source packages, and their
/// description for the repository.
pub fn package_build(
    distribution: &Distribution,
    parameters: &Value,
    arch: &str,
) -> Result<Files, String> {
    let name = word(parameters, "package");
    let package = distribution
        .packages()
        .map(|(_, package)| package)
        .find(|package| package.name == name)
        .ok_or_else(|| format!("the distribution has no package {name}"))?;
    // The version comes with the job, which may be for an older commit
    // of the distribution than the one on disk now.
    let version = list(parameters, "distribution")
        .iter()
        .find(|release| {
            list(release, "packages")
                .iter()
                .any(|p| p.as_str() == Some(name))
        })
        .map(|release| word(release, "version"))
        .ok_or_else(|| format!("the job's distribution has no package {name}"))?;
    let scale = parameters
        .get("scale")
        .ok_or("the job has no \"scale\" parameter")?;
    let build = Build {
        package,
        name: format!("{}{}", word(scale, "prefix"), name.replace('_', "-")),
        version,
        revision: parameters
            .get("revision")
            .and_then(Value::as_i64)
            .ok_or("the job has no revision")?,
        suffix: word(scale, "suffix"),
        prefix: word(scale, "prefix"),
    };
    let (mut files, metadata) = match word(scale, "format") {
        "deb" => deb(&build, arch),
        "rpm" => rpm(&build, arch)?,
        other => return Err(format!("unknown package format {other:?}")),
    };
    files.push((
        "repo-metadata.json".to_string(),
        metadata.to_json().into_bytes(),
    ));
    Ok(files)
}

const DESCRIPTION: &str = "A package of the scale test\n \
    It holds nothing. Its description is a few lines long, as the descriptions\n \
    of real packages are, so that the index has the size of a real one. The\n \
    lines say nothing more than that, and there are four of them.";

fn deb(build: &Build<'_>, arch: &str) -> (Files, Value) {
    let name = &build.name;
    let full = format!("{}-{}{}", build.version, build.revision, build.suffix);
    let depends = build
        .package
        .depends
        .iter()
        .map(|dependency| build.binary_name(dependency))
        .chain(["libc6 (>= 2.38)".to_string(), "python3".to_string()])
        .collect::<Vec<_>>()
        .join(", ");
    let pair = |name: &str, value: &str| Value::strings([name, value]);
    let mut files = Files::new();
    let mut entries = Vec::new();
    for (package, extension) in [(name.clone(), "deb"), (format!("{name}-dbgsym"), "ddeb")] {
        let path = format!("packages/{package}_{full}_{arch}.{extension}");
        files.push((path.clone(), build.filler(&path)));
        entries.push(Value::object([
            ("file", Value::from(path)),
            ("kind", Value::from("binary")),
            (
                "fields",
                Value::Array(vec![
                    pair("Package", &package),
                    pair("Version", &full),
                    pair("Architecture", arch),
                    pair("Maintainer", "Scale Test <scale-test@example.invalid>"),
                    pair("Installed-Size", "412"),
                    pair("Depends", &depends),
                    pair("Section", "misc"),
                    pair("Priority", "optional"),
                    pair("Homepage", "https://scale.invalid/"),
                    pair("Description", DESCRIPTION),
                ]),
            ),
        ]));
    }
    // The source package, which every architecture's build makes.
    let parts = [
        format!("{name}_{}.orig.tar.gz", build.version),
        format!("{name}_{full}.debian.tar.xz"),
    ];
    let mut checksums = String::new();
    let mut listed = String::new();
    for part in &parts {
        let data = build.filler(part);
        checksums.push_str(&format!("\n {} {} {part}", hash(&data), data.len()));
        listed.push_str(&format!(
            "\n 00000000000000000000000000000000 {} {part}",
            data.len()
        ));
        files.push((format!("packages/{part}"), data));
    }
    let path = format!("packages/{name}_{full}.dsc");
    files.push((path.clone(), build.filler(&path)));
    entries.push(Value::object([
        ("file", Value::from(path)),
        ("kind", Value::from("source")),
        (
            "fields",
            Value::Array(vec![
                pair("Format", "3.0 (quilt)"),
                pair("Source", name),
                pair("Binary", name),
                pair("Architecture", "any"),
                pair("Version", &full),
                pair("Maintainer", "Scale Test <scale-test@example.invalid>"),
                pair("Homepage", "https://scale.invalid/"),
                pair("Standards-Version", "3.9.2"),
                pair("Build-Depends", &format!("debhelper (>= 9.0.0), {depends}")),
                pair(
                    "Package-List",
                    &format!("\n {name} deb misc optional arch=any"),
                ),
                pair("Checksums-Sha256", &checksums),
                pair("Files", &listed),
            ]),
        ),
    ]));
    let metadata = Value::object([
        ("version", Value::Int(1)),
        ("format", Value::from("deb")),
        ("packages", Value::Array(entries)),
    ]);
    (files, metadata)
}

/// One element of createrepo_c's XML, as the coordinator takes it.
fn node(tag: &str, attrs: &[(&str, &str)], text: &str, children: Vec<Value>) -> Value {
    Value::object([
        ("tag", Value::from(tag)),
        (
            "attrs",
            Value::Array(attrs.iter().map(|(n, v)| Value::strings([n, v])).collect()),
        ),
        ("text", Value::from(text)),
        ("children", Value::Array(children)),
    ])
}

fn leaf(tag: &str, text: &str) -> Value {
    node(tag, &[], text, Vec::new())
}

fn rpm(build: &Build<'_>, arch: &str) -> Result<(Files, Value), String> {
    let rpm_arch = match arch {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        other => return Err(format!("no RPM name for architecture {other}")),
    };
    let name = &build.name;
    let release = format!("{}{}", build.revision, build.suffix);
    let source_rpm = format!("{name}-{}-{release}.src.rpm", build.version);
    let version = || {
        node(
            "version",
            &[("epoch", "0"), ("ver", build.version), ("rel", &release)],
            "",
            Vec::new(),
        )
    };
    let mut files = Files::new();
    let mut entries = Vec::new();
    for (package, package_arch) in [
        (name.clone(), rpm_arch),
        (format!("{name}-debuginfo"), rpm_arch),
        (name.clone(), "src"),
    ] {
        let path = format!(
            "packages/{package}-{}-{release}.{package_arch}.rpm",
            build.version
        );
        let data = build.filler(&path);
        let id = hash(&data);
        let requires = build
            .package
            .depends
            .iter()
            .map(|dependency| build.binary_name(dependency))
            .chain(["libc.so.6()(64bit)".to_string(), "python3".to_string()])
            .map(|name| node("rpm:entry", &[("name", &name)], "", Vec::new()))
            .collect();
        let provides = vec![node(
            "rpm:entry",
            &[
                ("name", &package),
                ("flags", "EQ"),
                ("epoch", "0"),
                ("ver", build.version),
                ("rel", &release),
            ],
            "",
            Vec::new(),
        )];
        let primary = node(
            "package",
            &[("type", "rpm")],
            "",
            vec![
                leaf("name", &package),
                leaf("arch", package_arch),
                version(),
                node(
                    "checksum",
                    &[("type", "sha256"), ("pkgid", "YES")],
                    &id,
                    Vec::new(),
                ),
                leaf("summary", "A package of the scale test"),
                leaf("description", DESCRIPTION),
                leaf("packager", "Scale Test <scale-test@example.invalid>"),
                leaf("url", "https://scale.invalid/"),
                node(
                    "time",
                    &[("file", "1795000000"), ("build", "1795000000")],
                    "",
                    Vec::new(),
                ),
                node(
                    "size",
                    &[
                        ("package", &data.len().to_string()),
                        ("installed", "421888"),
                        ("archive", "424000"),
                    ],
                    "",
                    Vec::new(),
                ),
                node("location", &[("href", &path)], "", Vec::new()),
                node(
                    "format",
                    &[],
                    "",
                    vec![
                        leaf("rpm:license", "Apache License 2.0"),
                        leaf("rpm:vendor", "Scale Test"),
                        leaf("rpm:group", "Development/Libraries"),
                        leaf("rpm:buildhost", "scale.invalid"),
                        leaf("rpm:sourcerpm", &source_rpm),
                        node(
                            "rpm:header-range",
                            &[("start", "4504"), ("end", "9000")],
                            "",
                            Vec::new(),
                        ),
                        node("rpm:provides", &[], "", provides),
                        node("rpm:requires", &[], "", requires),
                    ],
                ),
            ],
        );
        let same = [
            ("pkgid", id.as_str()),
            ("name", package.as_str()),
            ("arch", package_arch),
        ];
        // Two dozen files, which is few for a real package.
        let mut listed = vec![version()];
        for index in 0..24 {
            listed.push(leaf(
                "file",
                &format!(
                    "/opt/ros/rolling/share/{}/file_{index:02}.cmake",
                    build.package.name
                ),
            ));
        }
        let changelog = node(
            "changelog",
            &[
                ("author", "Scale Test <scale-test@example.invalid>"),
                ("date", "1795000000"),
            ],
            "- Autogenerated",
            Vec::new(),
        );
        entries.push(Value::object([
            ("file", Value::from(path.as_str())),
            ("primary", primary),
            ("filelists", node("package", &same, "", listed)),
            (
                "other",
                node("package", &same, "", vec![version(), changelog]),
            ),
        ]));
        files.push((path, data));
    }
    let metadata = Value::object([
        ("version", Value::Int(1)),
        ("format", Value::from("rpm")),
        ("packages", Value::Array(entries)),
    ]);
    Ok((files, metadata))
}
