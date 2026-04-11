use crate::error::Error;
use crate::manifest::deserialize_one_or_many;
use rndk::manifest::Activity;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub(crate) struct AndroidContributions {
    pub(crate) java_sources: Vec<PathBuf>,
    pub(crate) activities: Vec<Activity>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    resolve: Option<CargoResolve>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    id: String,
    name: String,
    manifest_path: PathBuf,
    #[serde(default)]
    metadata: Option<CargoPackageMetadata>,
}

#[derive(Debug, Deserialize)]
struct CargoResolve {
    root: Option<String>,
    nodes: Vec<CargoResolveNode>,
}

#[derive(Debug, Deserialize)]
struct CargoResolveNode {
    id: String,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    deps: Vec<CargoResolveNodeDep>,
}

#[derive(Debug, Deserialize)]
struct CargoResolveNodeDep {
    pkg: String,
}

#[derive(Debug, Default, Deserialize)]
struct CargoPackageMetadata {
    #[serde(default)]
    android: Option<AndroidContributionMetadata>,
}

#[derive(Debug, Default, Deserialize)]
struct AndroidContributionMetadata {
    #[serde(default)]
    cargo_rapk: Option<CargoRapkContributionMetadata>,
}

#[derive(Debug, Default, Deserialize)]
struct CargoRapkContributionMetadata {
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_one_or_many")]
    java_sources: Vec<PathBuf>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_one_or_many")]
    activities: Vec<Activity>,
}

pub(crate) fn collect_android_contributions(
    manifest_path: &Path,
) -> Result<AndroidContributions, Error> {
    let output = std::process::Command::new("cargo")
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(manifest_path)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let msg = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "failed to collect cargo metadata".to_string()
        };
        return Err(crate::error::Error::MetadataCommandFailed(msg));
    }

    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout)?;
    Ok(collect_from_metadata(metadata, manifest_path))
}

fn collect_from_metadata(metadata: CargoMetadata, manifest_path: &Path) -> AndroidContributions {
    let manifest_path = dunce::simplified(manifest_path).to_owned();
    let mut reachable_package_ids = HashSet::new();

    if let Some(resolve) = &metadata.resolve {
        let mut id_by_manifest_path = HashMap::new();
        let mut ids_by_name = HashMap::<String, Vec<String>>::new();

        for package in &metadata.packages {
            id_by_manifest_path.insert(
                dunce::simplified(&package.manifest_path).to_owned(),
                package.id.clone(),
            );
            ids_by_name
                .entry(package.name.clone())
                .or_default()
                .push(package.id.clone());
        }

        let root_id = resolve.root.clone().or_else(|| {
            id_by_manifest_path
                .get(&manifest_path)
                .cloned()
                .or_else(|| {
                    // Fall back to package name from the manifest file name.
                    manifest_path
                        .parent()
                        .and_then(|dir| dir.file_name())
                        .and_then(|name| name.to_str())
                        .and_then(|name| ids_by_name.get(name))
                        .and_then(|ids| ids.first())
                        .cloned()
                })
        });

        if let Some(root_id) = root_id {
            let adjacency = resolve
                .nodes
                .iter()
                .map(|node| {
                    let deps = if node.dependencies.is_empty() {
                        node.deps
                            .iter()
                            .map(|dep| dep.pkg.clone())
                            .collect::<Vec<_>>()
                    } else {
                        node.dependencies.clone()
                    };
                    (node.id.clone(), deps)
                })
                .collect::<HashMap<_, _>>();

            let mut stack = vec![root_id];
            while let Some(current) = stack.pop() {
                if !reachable_package_ids.insert(current.clone()) {
                    continue;
                }
                if let Some(deps) = adjacency.get(&current) {
                    for dep in deps {
                        stack.push(dep.clone());
                    }
                }
            }
        }
    }

    let mut java_sources = Vec::new();
    let mut java_source_set = HashSet::new();
    let mut activity_names = HashSet::new();
    let mut activities = Vec::new();

    for package in metadata.packages {
        if !reachable_package_ids.is_empty() && !reachable_package_ids.contains(&package.id) {
            continue;
        }

        let manifest_dir = package
            .manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let Some(metadata) = package.metadata else {
            continue;
        };

        let Some(android) = metadata.android else {
            continue;
        };
        let Some(contrib) = android.cargo_rapk else {
            continue;
        };

        for java_source in contrib.java_sources {
            let resolved = dunce::simplified(&manifest_dir.join(java_source)).to_owned();
            if java_source_set.insert(resolved.clone()) {
                java_sources.push(resolved);
            }
        }

        for activity in contrib.activities {
            if activity_names.insert(activity.name.clone()) {
                activities.push(activity);
            }
        }
    }

    AndroidContributions {
        java_sources,
        activities,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collect_from_metadata_json(
        metadata: serde_json::Value,
        manifest_path: &Path,
    ) -> AndroidContributions {
        let metadata: CargoMetadata = serde_json::from_value(metadata).expect("invalid json");

        collect_from_metadata(metadata, manifest_path)
    }

    #[test]
    fn only_collects_reachable_packages() {
        let manifest = PathBuf::from("/ws/app/Cargo.toml");
        let metadata = json!({
            "packages": [
                {
                    "id": "app 0.1.0 (path+file:///ws/app)",
                    "name": "app",
                    "manifest_path": "/ws/app/Cargo.toml",
                    "metadata": {"android": {"cargo_rapk": {"java_sources": ["android"], "activities": []}}}
                },
                {
                    "id": "dep_a 0.1.0 (path+file:///ws/dep_a)",
                    "name": "dep_a",
                    "manifest_path": "/ws/dep_a/Cargo.toml",
                    "metadata": {"android": {"cargo_rapk": {"java_sources": ["android"], "activities": [{"name": "pkg.DepAActivity"}]}}}
                },
                {
                    "id": "dep_b 0.1.0 (path+file:///ws/dep_b)",
                    "name": "dep_b",
                    "manifest_path": "/ws/dep_b/Cargo.toml",
                    "metadata": {"android": {"cargo_rapk": {"java_sources": ["android"], "activities": [{"name": "pkg.DepBActivity"}]}}}
                },
                {
                    "id": "unused 0.1.0 (path+file:///ws/unused)",
                    "name": "unused",
                    "manifest_path": "/ws/unused/Cargo.toml",
                    "metadata": {"android": {"cargo_rapk": {"java_sources": ["android"], "activities": [{"name": "pkg.UnusedActivity"}]}}}
                }
            ],
            "resolve": {
                "root": "app 0.1.0 (path+file:///ws/app)",
                "nodes": [
                    {
                        "id": "app 0.1.0 (path+file:///ws/app)",
                        "dependencies": [
                            "dep_a 0.1.0 (path+file:///ws/dep_a)",
                            "dep_b 0.1.0 (path+file:///ws/dep_b)"
                        ],
                        "deps": []
                    },
                    {"id": "dep_a 0.1.0 (path+file:///ws/dep_a)", "dependencies": [], "deps": []},
                    {"id": "dep_b 0.1.0 (path+file:///ws/dep_b)", "dependencies": [], "deps": []},
                    {"id": "unused 0.1.0 (path+file:///ws/unused)", "dependencies": [], "deps": []}
                ]
            }
        });

        let contributions = collect_from_metadata_json(metadata, &manifest);
        assert_eq!(contributions.activities.len(), 2);
        assert!(
            contributions
                .activities
                .iter()
                .any(|activity| activity.name == "pkg.DepAActivity")
        );
        assert!(
            contributions
                .activities
                .iter()
                .any(|activity| activity.name == "pkg.DepBActivity")
        );
        assert!(
            contributions
                .activities
                .iter()
                .all(|activity| activity.name != "pkg.UnusedActivity")
        );
    }

    #[test]
    fn deduplicates_contributed_entries() {
        let manifest = PathBuf::from("/ws/app/Cargo.toml");
        let metadata = json!({
            "packages": [
                {
                    "id": "app 0.1.0 (path+file:///ws/app)",
                    "name": "app",
                    "manifest_path": "/ws/app/Cargo.toml",
                    "metadata": {"android": {"cargo_rapk": {"java_sources": ["android"], "activities": [{"name": "pkg.SharedActivity"}]}}}
                },
                {
                    "id": "dep 0.1.0 (path+file:///ws/dep)",
                    "name": "dep",
                    "manifest_path": "/ws/dep/Cargo.toml",
                    "metadata": {"android": {"cargo_rapk": {"java_sources": ["android"], "activities": [{"name": "pkg.SharedActivity"}]}}}
                }
            ],
            "resolve": {
                "root": "app 0.1.0 (path+file:///ws/app)",
                "nodes": [
                    {
                        "id": "app 0.1.0 (path+file:///ws/app)",
                        "dependencies": ["dep 0.1.0 (path+file:///ws/dep)"],
                        "deps": []
                    },
                    {"id": "dep 0.1.0 (path+file:///ws/dep)", "dependencies": [], "deps": []}
                ]
            }
        });

        let contributions = collect_from_metadata_json(metadata, &manifest);
        assert_eq!(contributions.activities.len(), 1);
        assert_eq!(contributions.java_sources.len(), 2);
    }

    #[test]
    fn ignores_packages_with_null_metadata() {
        let manifest = PathBuf::from("/ws/app/Cargo.toml");
        let metadata = json!({
            "packages": [
                {
                    "id": "app 0.1.0 (path+file:///ws/app)",
                    "name": "app",
                    "manifest_path": "/ws/app/Cargo.toml",
                    "metadata": null
                },
                {
                    "id": "dep 0.1.0 (path+file:///ws/dep)",
                    "name": "dep",
                    "manifest_path": "/ws/dep/Cargo.toml",
                    "metadata": {
                        "android": {
                            "cargo_rapk": {
                                "java_sources": ["android"],
                                "activities": [{"name": "pkg.DepActivity"}]
                            }
                        }
                    }
                }
            ],
            "resolve": {
                "root": "app 0.1.0 (path+file:///ws/app)",
                "nodes": [
                    {
                        "id": "app 0.1.0 (path+file:///ws/app)",
                        "dependencies": ["dep 0.1.0 (path+file:///ws/dep)"],
                        "deps": []
                    },
                    {
                        "id": "dep 0.1.0 (path+file:///ws/dep)",
                        "dependencies": [],
                        "deps": []
                    }
                ]
            }
        });

        let contributions = collect_from_metadata_json(metadata, &manifest);
        assert_eq!(contributions.activities.len(), 1);
        assert_eq!(contributions.activities[0].name, "pkg.DepActivity");
    }

    #[test]
    fn includes_root_package_contributions() {
        let manifest = PathBuf::from("/ws/app/Cargo.toml");
        let metadata = json!({
            "packages": [
                {
                    "id": "app 0.1.0 (path+file:///ws/app)",
                    "name": "app",
                    "manifest_path": "/ws/app/Cargo.toml",
                    "metadata": {"android": {"cargo_rapk": {"java_sources": ["android"], "activities": [{"name": "pkg.RootActivity"}]}}}
                }
            ],
            "resolve": {
                "root": "app 0.1.0 (path+file:///ws/app)",
                "nodes": [
                    {
                        "id": "app 0.1.0 (path+file:///ws/app)",
                        "dependencies": [],
                        "deps": []
                    }
                ]
            }
        });

        let contributions = collect_from_metadata_json(metadata, &manifest);
        assert_eq!(contributions.activities.len(), 1);
        assert_eq!(contributions.activities[0].name, "pkg.RootActivity");
        assert_eq!(contributions.java_sources.len(), 1);
    }
}
