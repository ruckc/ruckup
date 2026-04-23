use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Deserialize;
use toml_edit::DocumentMut;

use crate::config::Config;
use crate::plugin::{DepType, Dependency, Plugin};

pub struct CargoPlugin {
    client: reqwest::Client,
}

impl CargoPlugin {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent("ruckup/0.1.0")
                .build()
                .expect("failed to build HTTP client"),
        }
    }
}

#[derive(Deserialize)]
struct CratesIoResponse {
    #[serde(rename = "crate")]
    krate: CrateInfo,
}

#[derive(Deserialize)]
struct CrateInfo {
    max_stable_version: String,
}

fn parse_cargo_toml(dir: &Path) -> Result<toml::Value> {
    let path = dir.join("Cargo.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let doc: toml::Value =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(doc)
}

fn extract_deps(table: &toml::Value, dep_type: DepType) -> Vec<Dependency> {
    let Some(table) = table.as_table() else {
        return vec![];
    };
    table
        .iter()
        .filter_map(|(name, value)| {
            let version = match value {
                toml::Value::String(v) => v.clone(),
                toml::Value::Table(t) => t.get("version")?.as_str()?.to_string(),
                _ => return None,
            };
            Some(Dependency::new(name.clone(), version, dep_type.clone()))
        })
        .collect()
}

async fn fetch_latest(client: &reqwest::Client, name: &str) -> Result<String> {
    let url = format!("https://crates.io/api/v1/crates/{name}");
    let resp: CratesIoResponse = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    // Strip semver build metadata (+...) — Cargo warns if it's in version requirements
    let version = resp.krate.max_stable_version;
    Ok(version.split('+').next().unwrap_or(&version).to_string())
}

#[async_trait]
impl Plugin for CargoPlugin {
    fn name(&self) -> &str {
        "Cargo"
    }

    fn file_name(&self) -> &str {
        "Cargo.toml"
    }

    async fn list_dependencies(&self, dir: &Path) -> Result<Vec<Dependency>> {
        let doc = parse_cargo_toml(dir)?;
        let mut deps = Vec::new();
        if let Some(d) = doc.get("dependencies") {
            deps.extend(extract_deps(d, DepType::Normal));
        }
        if let Some(d) = doc.get("dev-dependencies") {
            deps.extend(extract_deps(d, DepType::Dev));
        }
        if let Some(d) = doc.get("build-dependencies") {
            deps.extend(extract_deps(d, DepType::Build));
        }
        Ok(deps)
    }

    async fn check_updates(&self, dir: &Path, config: &Config) -> Result<Vec<Dependency>> {
        let deps = self.list_dependencies(dir).await?;
        let client = &self.client;
        let concurrency = config.cargo_concurrency;

        let pb = ProgressBar::new(deps.len() as u64);
        pb.set_style(
            ProgressStyle::with_template(
                "  {bar:30.cyan/dim} {pos}/{len} checked",
            )
            .unwrap()
            .progress_chars("━╸─"),
        );

        let results: Vec<Dependency> = stream::iter(deps)
            .map(|dep| async move {
                let mut dep = dep;
                match fetch_latest(client, &dep.name).await {
                    Ok(latest) => {
                        let satisfied = match (
                            semver::VersionReq::parse(&dep.current_version),
                            semver::Version::parse(&latest),
                        ) {
                            (Ok(req), Ok(ver)) => req.matches(&ver),
                            _ => latest == dep.current_version,
                        };
                        dep.satisfied = satisfied;
                        dep.latest_version = Some(latest);
                    }
                    Err(e) => eprintln!("  warning: failed to check {}: {e}", dep.name),
                }
                dep
            })
            .buffer_unordered(concurrency)
            .inspect(|_| pb.inc(1))
            .collect()
            .await;

        pb.finish_and_clear();
        Ok(results)
    }

    async fn update(&self, dir: &Path, dep_names: &[String], config: &Config) -> Result<()> {
        let path = dir.join("Cargo.toml");
        let content = std::fs::read_to_string(&path)?;
        let mut doc: DocumentMut = content.parse::<DocumentMut>()?;

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
                DepType::Dev => "dev-dependencies",
                DepType::Build => "build-dependencies",
                DepType::Optional => "dependencies",
            };
            if let Some(table) = doc.get_mut(section).and_then(|t| t.as_table_mut()) {
                if let Some(entry) = table.get_mut(&dep.name) {
                    match entry {
                        toml_edit::Item::Value(toml_edit::Value::String(s)) => {
                            *s = toml_edit::Formatted::new(latest.clone());
                        }
                        toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => {
                            if let Some(v) = t.get_mut("version") {
                                *v = toml_edit::Value::String(toml_edit::Formatted::new(
                                    latest.clone(),
                                ));
                            }
                        }
                        toml_edit::Item::Table(t) => {
                            t["version"] = toml_edit::value(latest.as_str());
                        }
                        _ => {}
                    }
                }
            }
        }

        std::fs::write(&path, doc.to_string())?;
        Ok(())
    }
}
