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

fn has_supported_pyproject_dependencies(doc: &toml::Value) -> bool {
    doc.get("project")
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
            .is_some()
        || doc.get("dependency-groups").is_some()
        || doc
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("dependencies"))
            .is_some()
        || doc
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("dev-dependencies"))
            .is_some()
        || doc
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("group"))
            .is_some()
}

fn version_with_preserved_prefix(current: &str, latest: &str, preserve: bool) -> String {
    if preserve {
        let prefix: String = current
            .chars()
            .take_while(|c| !c.is_ascii_digit())
            .collect();
        format!("{prefix}{latest}")
    } else {
        latest.to_string()
    }
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

fn extract_poetry_dependency(
    name: &str,
    value: &toml::Value,
    default_type: DepType,
) -> Option<Dependency> {
    if name == "python" {
        return None;
    }

    match value {
        toml::Value::String(version) => Some(Dependency::new(
            name.to_string(),
            version.clone(),
            default_type,
        )),
        toml::Value::Table(table) => {
            let version = table.get("version")?.as_str()?.to_string();
            let dep_type = if table
                .get("optional")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                DepType::Optional
            } else {
                default_type
            };
            Some(Dependency::new(name.to_string(), version, dep_type))
        }
        _ => None,
    }
}

fn extract_poetry_deps(table: &toml::value::Table, dep_type: DepType) -> Vec<Dependency> {
    table
        .iter()
        .filter_map(|(name, value)| extract_poetry_dependency(name, value, dep_type.clone()))
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
            return has_supported_pyproject_dependencies(&doc);
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

        // [tool.poetry.dependencies]
        if let Some(table) = doc
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("dependencies"))
            .and_then(|d| d.as_table())
        {
            deps.extend(extract_poetry_deps(table, DepType::Normal));
        }

        // [tool.poetry.dev-dependencies] (legacy Poetry)
        if let Some(table) = doc
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("dev-dependencies"))
            .and_then(|d| d.as_table())
        {
            deps.extend(extract_poetry_deps(table, DepType::Dev));
        }

        // [tool.poetry.group.<name>.dependencies]
        if let Some(groups) = doc
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("group"))
            .and_then(|g| g.as_table())
        {
            for (group, entry) in groups {
                if let Some(table) = entry.get("dependencies").and_then(|d| d.as_table()) {
                    let dep_type = if group == "dev" {
                        DepType::Dev
                    } else {
                        DepType::Optional
                    };
                    deps.extend(extract_poetry_deps(table, dep_type));
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

        fn update_poetry_table(
            table: &mut toml_edit::Table,
            updates: &[Dependency],
            preserve: bool,
        ) {
            for (name, item) in table.iter_mut() {
                if name.get() == "python" {
                    continue;
                }
                let Some(dep) = updates.iter().find(|d| d.name == name.get()) else {
                    continue;
                };
                let Some(latest) = dep.latest_version.as_deref() else {
                    continue;
                };

                if let Some(current) = item.as_str() {
                    let new_version = version_with_preserved_prefix(current, latest, preserve);
                    *item = toml_edit::value(new_version);
                    continue;
                }

                let Some(value) = item.as_value_mut() else {
                    continue;
                };
                let Some(inline) = value.as_inline_table_mut() else {
                    continue;
                };
                let Some(current) = inline.get("version").and_then(|value| value.as_str()) else {
                    continue;
                };
                let new_version = version_with_preserved_prefix(current, latest, preserve);
                inline.insert("version", toml_edit::Value::from(new_version));
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

        // [tool.poetry.dependencies]
        if let Some(table) = doc
            .get_mut("tool")
            .and_then(|t| t.get_mut("poetry"))
            .and_then(|p| p.get_mut("dependencies"))
            .and_then(|d| d.as_table_mut())
        {
            update_poetry_table(table, &to_update, preserve_range);
        }

        // [tool.poetry.dev-dependencies]
        if let Some(table) = doc
            .get_mut("tool")
            .and_then(|t| t.get_mut("poetry"))
            .and_then(|p| p.get_mut("dev-dependencies"))
            .and_then(|d| d.as_table_mut())
        {
            update_poetry_table(table, &to_update, preserve_range);
        }

        // [tool.poetry.group.<name>.dependencies]
        if let Some(groups) = doc
            .get_mut("tool")
            .and_then(|t| t.get_mut("poetry"))
            .and_then(|p| p.get_mut("group"))
            .and_then(|g| g.as_table_mut())
        {
            for (_group, entry) in groups.iter_mut() {
                if let Some(table) = entry.get_mut("dependencies").and_then(|d| d.as_table_mut())
                {
                    update_poetry_table(table, &to_update, preserve_range);
                }
            }
        }

        std::fs::write(&path, doc.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DepType, extract_poetry_deps, has_supported_pyproject_dependencies, parse_pep508,
        version_with_preserved_prefix,
    };

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

    #[test]
    fn poetry_dependencies_are_extracted_and_python_is_skipped() {
        let doc: toml::Value = toml::from_str(
            r#"
[tool.poetry.dependencies]
python = "^3.12"
requests = "^2.32"
httpx = { version = "^0.28", optional = true }
"#,
        )
        .expect("failed to parse toml");

        let table = doc
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("dependencies"))
            .and_then(|d| d.as_table())
            .expect("missing poetry dependencies");

        let deps = extract_poetry_deps(table, DepType::Normal);
        assert_eq!(deps.len(), 2);

        let requests = deps
            .iter()
            .find(|dep| dep.name == "requests")
            .expect("missing requests dependency");
        assert_eq!(requests.current_version, "^2.32");
        assert_eq!(requests.dep_type, DepType::Normal);

        let httpx = deps
            .iter()
            .find(|dep| dep.name == "httpx")
            .expect("missing httpx dependency");
        assert_eq!(httpx.current_version, "^0.28");
        assert_eq!(httpx.dep_type, DepType::Optional);
    }

    #[test]
    fn detect_recognizes_poetry_and_dependency_groups() {
        let poetry_doc: toml::Value = toml::from_str(
            r#"
[tool.poetry.group.dev.dependencies]
pytest = "^8.3"
"#,
        )
        .expect("failed to parse poetry toml");
        assert!(has_supported_pyproject_dependencies(&poetry_doc));

        let groups_doc: toml::Value = toml::from_str(
            r#"
[dependency-groups]
lint = ["ruff>=0.11"]
"#,
        )
        .expect("failed to parse dependency groups toml");
        assert!(has_supported_pyproject_dependencies(&groups_doc));
    }

    #[test]
    fn version_prefix_preservation_matches_existing_behavior() {
        assert_eq!(version_with_preserved_prefix("^2.0", "3.1.4", true), "^3.1.4");
        assert_eq!(version_with_preserved_prefix("^2.0", "3.1.4", false), "3.1.4");
    }
}
