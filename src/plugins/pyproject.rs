use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use toml_edit::DocumentMut;

use crate::config::Config;
use crate::http;
use crate::plugin::{DepType, Dependency, Plugin};
use crate::progress;

use futures::stream::{self, StreamExt};

pub struct PyprojectPlugin {
    client: reqwest::Client,
}

impl PyprojectPlugin {
    pub fn new() -> Self {
        Self {
            client: http::default_client().expect("failed to build HTTP client"),
        }
    }
}

#[derive(Deserialize)]
struct PypiResponse {
    info: PypiInfo,
}

#[derive(Deserialize)]
struct PypiInfo {
    version: String,
}

/// Parse a PEP 508 dependency string like "requests>=2.28.0" into (name, version_spec).
fn parse_pep508(spec: &str) -> Option<(String, String)> {
    // Strip extras like [security] and environment markers after ;
    let spec = spec.split(';').next().unwrap_or(spec).trim();

    // Find where the version specifier starts
    let version_start = spec.find(['>', '<', '=', '!', '~']);

    match version_start {
        Some(pos) => {
            let name = spec[..pos].trim().trim_end_matches('[').to_string();
            // Clean the name of any extras
            let name = name.split('[').next().unwrap_or(&name).trim().to_string();
            let version = spec[pos..].trim().to_string();
            Some((name, version))
        }
        None => {
            let name = spec.split('[').next().unwrap_or(spec).trim().to_string();
            if name.is_empty() {
                None
            } else {
                Some((name, "*".to_string()))
            }
        }
    }
}

fn parse_pyproject(dir: &Path) -> Result<toml::Value> {
    let path = dir.join("pyproject.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let doc: toml::Value =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(doc)
}

fn extract_deps_from_array(arr: &[toml::Value], dep_type: DepType) -> Vec<Dependency> {
    arr.iter()
        .filter_map(|v| {
            let s = v.as_str()?;
            let (name, version) = parse_pep508(s)?;
            Some(Dependency::new(name, version, dep_type.clone()))
        })
        .collect()
}

async fn fetch_latest(client: &reqwest::Client, name: &str) -> Result<String> {
    let url = format!("https://pypi.org/pypi/{name}/json");
    let resp: PypiResponse = http::get_json_with_retries(|| client.get(&url)).await?;
    Ok(resp.info.version)
}

#[async_trait]
impl Plugin for PyprojectPlugin {
    fn name(&self) -> &str {
        "pyproject"
    }

    fn file_name(&self) -> &str {
        "pyproject.toml"
    }

    fn detect(&self, dir: &Path) -> bool {
        let path = dir.join("pyproject.toml");
        if !path.is_file() {
            return false;
        }
        // Only activate if there are Python dependencies (not just a Cargo workspace pyproject)
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(doc) = content.parse::<toml::Value>()
        {
            return doc
                .get("project")
                .and_then(|p| p.get("dependencies"))
                .is_some()
                || doc
                    .get("project")
                    .and_then(|p| p.get("optional-dependencies"))
                    .is_some()
                || doc
                    .get("tool")
                    .and_then(|t| t.get("uv"))
                    .and_then(|u| u.get("dev-dependencies"))
                    .is_some();
        }
        false
    }

    async fn list_dependencies(&self, dir: &Path) -> Result<Vec<Dependency>> {
        let doc = parse_pyproject(dir)?;
        let mut deps = Vec::new();

        // [project.dependencies]
        if let Some(arr) = doc
            .get("project")
            .and_then(|p| p.get("dependencies"))
            .and_then(|d| d.as_array())
        {
            deps.extend(extract_deps_from_array(arr, DepType::Normal));
        }

        // [project.optional-dependencies.*]
        if let Some(opt) = doc
            .get("project")
            .and_then(|p| p.get("optional-dependencies"))
            .and_then(|o| o.as_table())
        {
            for (_group, arr) in opt {
                if let Some(arr) = arr.as_array() {
                    deps.extend(extract_deps_from_array(arr, DepType::Optional));
                }
            }
        }

        // [tool.uv.dev-dependencies] (uv-specific)
        if let Some(arr) = doc
            .get("tool")
            .and_then(|t| t.get("uv"))
            .and_then(|u| u.get("dev-dependencies"))
            .and_then(|d| d.as_array())
        {
            deps.extend(extract_deps_from_array(arr, DepType::Dev));
        }

        // [dependency-groups] (PEP 735)
        if let Some(groups) = doc.get("dependency-groups").and_then(|g| g.as_table()) {
            for (group, arr) in groups {
                if let Some(arr) = arr.as_array() {
                    let dep_type = if group == "dev" {
                        DepType::Dev
                    } else {
                        DepType::Optional
                    };
                    deps.extend(extract_deps_from_array(arr, dep_type));
                }
            }
        }

        Ok(deps)
    }

    async fn check_updates(&self, dir: &Path, config: &Config) -> Result<Vec<Dependency>> {
        let deps = self.list_dependencies(dir).await?;
        let client = &self.client;
        let concurrency = config.pypi_concurrency;

        let pb = progress::check_progress_bar(deps.len() as u64);

        let results: Vec<Dependency> = stream::iter(deps)
            .map(|dep| async move {
                let mut dep = dep;
                match fetch_latest(client, &dep.name).await {
                    Ok(latest) => {
                        let satisfied = match pep440_rs::Version::from_str(&latest) {
                            Ok(ver) => {
                                match pep440_rs::VersionSpecifiers::from_str(&dep.current_version) {
                                    Ok(specs) => specs.contains(&ver),
                                    Err(_) => {
                                        dep.current_version == "*" || latest == dep.current_version
                                    }
                                }
                            }
                            Err(_) => latest == dep.current_version,
                        };
                        dep.satisfied = satisfied;
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
        Ok(results)
    }

    async fn update(&self, dir: &Path, dep_names: &[String], config: &Config) -> Result<()> {
        let path = dir.join("pyproject.toml");
        let content = std::fs::read_to_string(&path)?;
        let mut doc: DocumentMut = content.parse::<DocumentMut>()?;

        let updates = self.check_updates(dir, config).await?;
        let to_update: Vec<_> = updates
            .into_iter()
            .filter(|d| d.has_update())
            .filter(|d| dep_names.is_empty() || dep_names.contains(&d.name))
            .collect();

        let preserve_range = config.preserve_range;

        // Helper: update a TOML array of PEP 508 strings in place
        fn update_array(arr: &mut toml_edit::Array, updates: &[Dependency], preserve: bool) {
            for item in arr.iter_mut() {
                let Some(s) = item.as_str() else { continue };
                let Some((name, _)) = parse_pep508(s) else {
                    continue;
                };
                if let Some(dep) = updates.iter().find(|d| d.name == name) {
                    let Some(ref latest) = dep.latest_version else {
                        continue;
                    };
                    // Preserve or strip the operator prefix
                    let new_spec = if preserve {
                        let old = s.to_string();
                        let op_end = old.find(|c: char| c.is_ascii_digit()).unwrap_or(old.len());
                        let prefix = &old[..op_end];
                        format!("{prefix}{latest}")
                    } else {
                        format!("{name}=={latest}")
                    };
                    *item = toml_edit::Value::String(toml_edit::Formatted::new(new_spec));
                }
            }
        }

        // [project.dependencies]
        if let Some(arr) = doc
            .get_mut("project")
            .and_then(|p| p.get_mut("dependencies"))
            .and_then(|d| d.as_array_mut())
        {
            update_array(arr, &to_update, preserve_range);
        }

        // [project.optional-dependencies.*]
        if let Some(opt) = doc
            .get_mut("project")
            .and_then(|p| p.get_mut("optional-dependencies"))
            .and_then(|o| o.as_table_mut())
        {
            for (_group, arr) in opt.iter_mut() {
                if let Some(arr) = arr.as_array_mut() {
                    update_array(arr, &to_update, preserve_range);
                }
            }
        }

        // [tool.uv.dev-dependencies]
        if let Some(arr) = doc
            .get_mut("tool")
            .and_then(|t| t.get_mut("uv"))
            .and_then(|u| u.get_mut("dev-dependencies"))
            .and_then(|d| d.as_array_mut())
        {
            update_array(arr, &to_update, preserve_range);
        }

        // [dependency-groups.*]
        if let Some(groups) = doc
            .get_mut("dependency-groups")
            .and_then(|g| g.as_table_mut())
        {
            for (_group, arr) in groups.iter_mut() {
                if let Some(arr) = arr.as_array_mut() {
                    update_array(arr, &to_update, preserve_range);
                }
            }
        }

        std::fs::write(&path, doc.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::parse_pep508;

    #[test]
    fn parse_pep508_with_version_specifier() {
        let parsed = parse_pep508("requests>=2.28.0").expect("failed to parse");
        assert_eq!(parsed.0, "requests");
        assert_eq!(parsed.1, ">=2.28.0");
    }

    #[test]
    fn parse_pep508_without_version_defaults_to_wildcard() {
        let parsed = parse_pep508("rich").expect("failed to parse");
        assert_eq!(parsed.0, "rich");
        assert_eq!(parsed.1, "*");
    }

    #[test]
    fn parse_pep508_strips_extras_and_markers() {
        let parsed = parse_pep508("uvicorn[standard]>=0.34; python_version >= '3.9'")
            .expect("failed to parse");
        assert_eq!(parsed.0, "uvicorn");
        assert_eq!(parsed.1, ">=0.34");
    }
}
