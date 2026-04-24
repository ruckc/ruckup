use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::config::Config;
use crate::http;
use crate::plugin::{DepType, Dependency, PeerConflict, Plugin};
use crate::progress;

use futures::stream::{self, StreamExt};

/// Detected JS package manager based on lock file presence.
#[derive(Debug, Clone, Copy)]
pub enum JsPackageManager {
    Npm,
    Pnpm,
    Yarn,
}

impl JsPackageManager {
    /// Detect which package manager is used by checking for lock files.
    pub fn detect(dir: &Path) -> Option<Self> {
        if dir.join("pnpm-lock.yaml").is_file() {
            Some(Self::Pnpm)
        } else if dir.join("yarn.lock").is_file() {
            Some(Self::Yarn)
        } else if dir.join("package-lock.json").is_file() {
            Some(Self::Npm)
        } else if dir.join("package.json").is_file() {
            // Fallback: package.json exists but no lock file
            Some(Self::Npm)
        } else {
            None
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
        }
    }

    pub fn lock_file(&self) -> &'static str {
        match self {
            Self::Npm => "package-lock.json",
            Self::Pnpm => "pnpm-lock.yaml",
            Self::Yarn => "yarn.lock",
        }
    }
}

pub struct NpmPlugin {
    client: reqwest::Client,
}

impl NpmPlugin {
    pub fn new() -> Self {
        Self {
            client: http::default_client().expect("failed to build HTTP client"),
        }
    }

    fn detect_pm(&self, dir: &Path) -> Option<JsPackageManager> {
        JsPackageManager::detect(dir)
    }
}

#[derive(Deserialize)]
struct NpmRegistryResponse {
    #[serde(rename = "dist-tags")]
    dist_tags: DistTags,
    #[serde(default)]
    versions: std::collections::HashMap<String, NpmVersionMeta>,
}

#[derive(Deserialize)]
struct DistTags {
    latest: String,
}

#[derive(Deserialize)]
struct NpmVersionMeta {
    #[serde(rename = "peerDependencies", default)]
    peer_dependencies: std::collections::HashMap<String, String>,
    #[serde(rename = "peerDependenciesMeta", default)]
    peer_dependencies_meta: std::collections::HashMap<String, PeerDepMeta>,
}

#[derive(Deserialize)]
struct PeerDepMeta {
    #[serde(default)]
    optional: bool,
}

fn parse_package_json(dir: &Path) -> Result<JsonValue> {
    let path = dir.join("package.json");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let doc: JsonValue = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(doc)
}

fn extract_deps(obj: &JsonValue, dep_type: DepType) -> Vec<Dependency> {
    let Some(map) = obj.as_object() else {
        return vec![];
    };
    map.iter()
        .filter_map(|(name, value)| {
            let version_str = value.as_str()?;
            Some(Dependency::new(
                name.clone(),
                version_str.to_string(),
                dep_type.clone(),
            ))
        })
        .collect()
}

async fn fetch_latest(
    client: &reqwest::Client,
    name: &str,
) -> Result<(String, Vec<(String, String)>)> {
    let url = format!("https://registry.npmjs.org/{name}");
    let resp: NpmRegistryResponse = http::get_json_with_retries(|| {
        client
            .get(&url)
            .header("Accept", "application/vnd.npm.install-v1+json")
    })
    .await?;
    let latest = &resp.dist_tags.latest;
    let peer_deps = resp.versions.get(latest).map_or_else(Vec::new, |meta| {
        meta.peer_dependencies
            .iter()
            .filter(|(name, _)| {
                !meta
                    .peer_dependencies_meta
                    .get(*name)
                    .is_some_and(|m| m.optional)
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    });
    Ok((resp.dist_tags.latest, peer_deps))
}

#[async_trait]
impl Plugin for NpmPlugin {
    fn name(&self) -> &str {
        "npm"
    }

    fn file_name(&self) -> &str {
        "package.json"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("package.json").is_file()
    }

    fn display_name(&self, dir: &Path) -> String {
        let pm = self.detect_pm(dir);
        match pm {
            Some(pm) => {
                let lock = if dir.join(pm.lock_file()).is_file() {
                    format!(" + {}", pm.lock_file())
                } else {
                    String::new()
                };
                format!("{} (package.json{})", pm.name(), lock)
            }
            None => "npm (package.json)".to_string(),
        }
    }

    async fn list_dependencies(&self, dir: &Path) -> Result<Vec<Dependency>> {
        let doc = parse_package_json(dir)?;
        let mut deps = Vec::new();
        if let Some(d) = doc.get("dependencies") {
            deps.extend(extract_deps(d, DepType::Normal));
        }
        if let Some(d) = doc.get("devDependencies") {
            deps.extend(extract_deps(d, DepType::Dev));
        }
        if let Some(d) = doc.get("optionalDependencies") {
            deps.extend(extract_deps(d, DepType::Optional));
        }
        Ok(deps)
    }

    async fn check_updates(&self, dir: &Path, config: &Config) -> Result<Vec<Dependency>> {
        let deps = self.list_dependencies(dir).await?;
        let client = &self.client;
        let concurrency = config.npm_concurrency;

        let pb = progress::check_progress_bar(deps.len() as u64);

        let mut results: Vec<Dependency> = stream::iter(deps)
            .map(|dep| async move {
                let mut dep = dep;
                match fetch_latest(client, &dep.name).await {
                    Ok((latest, peer_deps)) => {
                        let satisfied = match (
                            node_semver::Range::parse(&dep.current_version),
                            node_semver::Version::parse(&latest),
                        ) {
                            (Ok(range), Ok(ver)) => range.satisfies(&ver),
                            _ => latest == dep.current_version,
                        };
                        dep.satisfied = satisfied;
                        dep.peer_deps = peer_deps;
                        dep.latest_version = Some(latest);
                    }
                    Err(e) => {
                        dep.check_failed = true;
                        eprintln!("  warning: failed to check {}: {e}", dep.name)
                    }
                }
                dep
            })
            .buffer_unordered(concurrency)
            .inspect(|_| pb.inc(1))
            .collect()
            .await;

        pb.finish_and_clear();

        // Resolve peer dependency conflicts
        let name_to_idx: std::collections::HashMap<&str, usize> = results
            .iter()
            .enumerate()
            .map(|(i, d)| (d.name.as_str(), i))
            .collect();

        let mut conflicts: Vec<(usize, PeerConflict)> = Vec::new();

        for dep in &results {
            let blocker_version = dep
                .latest_version
                .as_deref()
                .unwrap_or(&dep.current_version)
                .to_string();

            for (peer_name, peer_range) in &dep.peer_deps {
                if let Some(&j) = name_to_idx.get(peer_name.as_str()) {
                    let target = &results[j];
                    let target_version = target
                        .latest_version
                        .as_deref()
                        .unwrap_or(&target.current_version);

                    let satisfied = match (
                        node_semver::Range::parse(peer_range),
                        node_semver::Version::parse(target_version),
                    ) {
                        (Ok(range), Ok(ver)) => range.satisfies(&ver),
                        _ => true,
                    };

                    if !satisfied {
                        conflicts.push((
                            j,
                            PeerConflict {
                                blocker: dep.name.clone(),
                                blocker_version: blocker_version.clone(),
                                required_range: peer_range.clone(),
                            },
                        ));
                    }
                }
            }
        }

        for (idx, conflict) in conflicts {
            results[idx].held_back_by.push(conflict);
        }

        Ok(results)
    }

    async fn update(&self, dir: &Path, dep_names: &[String], config: &Config) -> Result<()> {
        let path = dir.join("package.json");
        let content = std::fs::read_to_string(&path)?;
        let mut doc: JsonValue = serde_json::from_str(&content)?;

        let updates = self.check_updates(dir, config).await?;
        let to_update: Vec<_> = updates
            .into_iter()
            .filter(|d| d.has_update())
            .filter(|d| dep_names.is_empty() || dep_names.contains(&d.name))
            .collect();

        for dep in &to_update {
            let Some(ref latest) = dep.latest_version else {
                continue;
            };
            let section = match dep.dep_type {
                DepType::Normal => "dependencies",
                DepType::Dev => "devDependencies",
                DepType::Optional => "optionalDependencies",
                DepType::Build => continue,
            };
            if let Some(obj) = doc.get_mut(section).and_then(|v| v.as_object_mut())
                && let Some(entry) = obj.get_mut(&dep.name)
            {
                let new_value = if config.preserve_range {
                    // Preserve the range prefix (^, ~, >=, etc.)
                    let old = entry.as_str().unwrap_or("");
                    let prefix: String = old.chars().take_while(|c| !c.is_ascii_digit()).collect();
                    format!("{prefix}{latest}")
                } else {
                    latest.clone()
                };
                *entry = JsonValue::String(new_value);
            }
        }

        let output = serde_json::to_string_pretty(&doc)?;
        std::fs::write(&path, format!("{output}\n"))?;
        Ok(())
    }
}
