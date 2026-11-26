// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! A made-up ROS distribution with the shape of a real one.
//!
//! The shape is taken from Rolling as its rosdistro cache had it on
//! 2026-11-26: 1,600 packages in 642 repositories, 6.5 dependencies and
//! 2.2 test dependencies a package on average, a longest dependency chain
//! of 42 packages, and a lowest package that 91% of the others depend on,
//! directly or not.

use std::collections::{BTreeMap, BTreeSet};

use ci_crypto::{hex_encode, sha256};
use ci_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub depends: Vec<String>,
    pub test_depends: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub name: String,
    pub version: String,
    pub packages: Vec<Package>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distribution {
    pub repositories: Vec<Repository>,
}

/// SplitMix64: small, and the same on every machine.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Between 0 and 1, never 0.
    fn unit(&mut self) -> f64 {
        ((self.next() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }

    /// A whole number with an exponential spread around `mean`.
    fn spread(&mut self, mean: f64) -> usize {
        (-self.unit().ln() * mean) as usize
    }
}

/// How many packages a repository holds, as in Rolling: two thirds hold
/// one, and a few hold dozens.
fn repository_size(random: &mut Random) -> usize {
    let u = random.unit();
    match u {
        u if u < 0.67 => 1,
        u if u < 0.775 => 2,
        u if u < 0.831 => 3,
        u if u < 0.868 => 4,
        u if u < 0.893 => 5,
        u if u < 0.916 => 6,
        u if u < 0.953 => 7 + (random.next() % 4) as usize,
        u if u < 0.989 => 11 + (random.next() % 10) as usize,
        _ => 21 + (random.next() % 21) as usize,
    }
}

/// How many of Rolling's packages have a longest chain of dependencies
/// below them of each length, from none upwards.
const LEVELS: [usize; 42] = [
    64, 21, 26, 10, 28, 113, 39, 28, 24, 23, 13, 10, 22, 10, 9, 3, 3, 2, 2, 62, 80, 57, 50, 14, 26,
    12, 63, 154, 95, 125, 112, 112, 65, 50, 26, 11, 10, 7, 9, 4, 5, 1,
];

/// The first levels each hold one package that nearly everything above
/// stands on, as ament_package and the ament_cmake packages are.
const BASE_LEVELS: usize = 6;

pub fn generate(packages: usize, seed: u64) -> Distribution {
    let mut random = Random(seed);
    let name = |index: usize| format!("pkg_{index:05}");
    // Where each level starts, with the levels in Rolling's proportions.
    let total: usize = LEVELS.iter().sum();
    let mut starts = vec![0];
    let mut below = 0;
    for width in LEVELS {
        below += width;
        let end = (below * packages).div_ceil(total).min(packages);
        if end > *starts.last().expect("starts is never empty") {
            starts.push(end);
        }
    }
    let mut all = Vec::with_capacity(packages);
    for level in 0..starts.len() - 1 {
        for index in starts[level]..starts[level + 1] {
            let mut depends = BTreeSet::new();
            let mut test_depends = BTreeSet::new();
            if level > 0 {
                // One package of the level just below, which is what
                // puts this one on its level: the base package of that
                // level for most, while there is one.
                let (from, to) = (starts[level - 1], starts[level]);
                let base = level <= BASE_LEVELS && random.unit() < 0.7;
                depends.insert(if base {
                    from
                } else {
                    from + (random.next() % (to - from) as u64) as usize
                });
                if level > BASE_LEVELS && random.unit() < 0.7 {
                    depends.insert(starts[BASE_LEVELS - 1]);
                }
                for _ in 0..random.spread(5.8).min(46) {
                    depends.insert((random.next() % to as u64) as usize);
                }
                for _ in 0..random.spread(2.7) {
                    let other = (random.next() % to as u64) as usize;
                    if !depends.contains(&other) {
                        test_depends.insert(other);
                    }
                }
            }
            all.push(Package {
                name: name(index),
                depends: depends.into_iter().map(name).collect(),
                test_depends: test_depends.into_iter().map(name).collect(),
            });
        }
    }
    let mut repositories = Vec::new();
    let mut rest = all.into_iter().peekable();
    while rest.peek().is_some() {
        let size = repository_size(&mut random);
        repositories.push(Repository {
            name: format!("repo_{:04}", repositories.len()),
            version: "1.0.0".to_string(),
            packages: rest.by_ref().take(size).collect(),
        });
    }
    Distribution { repositories }
}

impl Distribution {
    pub fn packages(&self) -> impl Iterator<Item = (&Repository, &Package)> {
        self.repositories
            .iter()
            .flat_map(|repository| repository.packages.iter().map(move |p| (repository, p)))
    }

    pub fn to_json(&self) -> String {
        let names = |list: &[String]| Value::strings(list);
        Value::object([(
            "repositories",
            Value::Array(
                self.repositories
                    .iter()
                    .map(|repository| {
                        Value::object([
                            ("name", Value::from(repository.name.as_str())),
                            ("version", Value::from(repository.version.as_str())),
                            (
                                "packages",
                                Value::Array(
                                    repository
                                        .packages
                                        .iter()
                                        .map(|package| {
                                            Value::object([
                                                ("name", Value::from(package.name.as_str())),
                                                ("depends", names(&package.depends)),
                                                ("test_depends", names(&package.test_depends)),
                                            ])
                                        })
                                        .collect(),
                                ),
                            ),
                        ])
                    })
                    .collect(),
            ),
        )])
        .to_json()
    }

    pub fn parse(text: &str) -> Result<Distribution, String> {
        let value = ci_json::parse(text).map_err(|e| e.to_string())?;
        let list = |value: &Value, name: &str| -> Result<Vec<Value>, String> {
            value
                .get(name)
                .and_then(Value::as_array)
                .map(<[Value]>::to_vec)
                .ok_or_else(|| format!("\"{name}\" is missing"))
        };
        let word = |value: &Value, name: &str| -> Result<String, String> {
            value
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("\"{name}\" is missing"))
        };
        let words = |value: &Value, name: &str| -> Result<Vec<String>, String> {
            Ok(list(value, name)?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect())
        };
        let mut repositories = Vec::new();
        for repository in list(&value, "repositories")? {
            let mut packages = Vec::new();
            for package in list(&repository, "packages")? {
                packages.push(Package {
                    name: word(&package, "name")?,
                    depends: words(&package, "depends")?,
                    test_depends: words(&package, "test_depends")?,
                });
            }
            repositories.push(Repository {
                name: word(&repository, "name")?,
                version: word(&repository, "version")?,
                packages,
            });
        }
        Ok(Distribution { repositories })
    }

    /// What stands in for the rosdistro's commit: it changes whenever
    /// the distribution does.
    pub fn commit(&self) -> String {
        hex_encode(&sha256(self.to_json().as_bytes()))[..40].to_string()
    }

    /// Raises the last number of the version of the repository that
    /// holds `package`. Returns the repository's name and new version.
    pub fn bump(&mut self, package: &str) -> Result<(String, String), String> {
        let repository = self
            .repositories
            .iter_mut()
            .find(|repository| repository.packages.iter().any(|p| p.name == package))
            .ok_or_else(|| format!("there is no package {package}"))?;
        let (front, last) = repository
            .version
            .rsplit_once('.')
            .ok_or("the version has no dots")?;
        let last: u64 = last.parse().map_err(|_| "the version is not numbers")?;
        repository.version = format!("{front}.{}", last + 1);
        Ok((repository.name.clone(), repository.version.clone()))
    }

    /// The packages that must be built again when the repository that
    /// holds `package` gets a new version: its own packages, and all
    /// that depend on any of them, directly or not. With `tests`, test
    /// dependencies count.
    pub fn rebuilt_with(&self, package: &str, tests: bool) -> BTreeSet<String> {
        let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (_, other) in self.packages() {
            let test_depends = other.test_depends.iter().filter(|_| tests);
            for dependency in other.depends.iter().chain(test_depends) {
                dependents.entry(dependency).or_default().push(&other.name);
            }
        }
        let mut todo: Vec<&str> = self
            .repositories
            .iter()
            .filter(|repository| repository.packages.iter().any(|p| p.name == package))
            .flat_map(|repository| repository.packages.iter().map(|p| p.name.as_str()))
            .collect();
        let mut found: BTreeSet<String> = todo.iter().map(|name| name.to_string()).collect();
        while let Some(name) = todo.pop() {
            for dependent in dependents.get(name).into_iter().flatten() {
                if found.insert(dependent.to_string()) {
                    todo.push(dependent);
                }
            }
        }
        found
    }

    /// A few lines on the shape of the distribution.
    pub fn describe(&self) -> String {
        let mut depth: BTreeMap<&str, usize> = BTreeMap::new();
        let (mut count, mut edges, mut test_edges) = (0, 0, 0);
        // Packages only depend on earlier ones, so one pass is enough.
        for (_, package) in self.packages() {
            let below = package
                .depends
                .iter()
                .filter_map(|dependency| depth.get(dependency.as_str()))
                .max()
                .map_or(0, |d| d + 1);
            depth.insert(&package.name, below);
            count += 1;
            edges += package.depends.len();
            test_edges += package.test_depends.len();
        }
        let mut per_level: BTreeMap<usize, usize> = BTreeMap::new();
        for level in depth.values() {
            *per_level.entry(*level).or_default() += 1;
        }
        let first = self.packages().next().map_or("", |(_, p)| p.name.as_str());
        format!(
            "{count} packages in {} repositories\n\
             {:.1} dependencies and {:.1} test dependencies a package\n\
             longest dependency chain: {} packages; widest level: {} packages\n\
             {} of the packages stand on {first}\n",
            self.repositories.len(),
            edges as f64 / count.max(1) as f64,
            test_edges as f64 / count.max(1) as f64,
            per_level.len(),
            per_level.values().max().copied().unwrap_or(0),
            self.rebuilt_with(first, false).len().saturating_sub(1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_distribution() {
        let first = generate(300, 7);
        assert_eq!(first, generate(300, 7));
        assert_ne!(first, generate(300, 8));
        assert_eq!(first.packages().count(), 300);
        assert_eq!(Distribution::parse(&first.to_json()).unwrap(), first);
    }

    #[test]
    fn packages_depend_only_on_earlier_ones() {
        let distribution = generate(500, 1);
        let mut seen = BTreeSet::new();
        for (_, package) in distribution.packages() {
            for dependency in package.depends.iter().chain(&package.test_depends) {
                assert!(
                    seen.contains(dependency),
                    "{} -> {dependency}",
                    package.name
                );
            }
            seen.insert(package.name.clone());
        }
    }

    #[test]
    fn a_new_version_changes_the_commit_and_names_what_follows() {
        let mut distribution = generate(200, 3);
        let before = distribution.commit();
        let (repository, version) = distribution.bump("pkg_00000").unwrap();
        assert_eq!(
            (repository.as_str(), version.as_str()),
            ("repo_0000", "1.0.1")
        );
        assert_ne!(distribution.commit(), before);
        let rebuilt = distribution.rebuilt_with("pkg_00000", false);
        assert!(rebuilt.contains("pkg_00000") && rebuilt.len() > 150);
        assert!(distribution.bump("nothing").is_err());
    }
}
