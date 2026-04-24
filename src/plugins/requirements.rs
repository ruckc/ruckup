use std::path::Path;
use std::str::FromStr;

use anyhow::Result;
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use serde::Deserialize;

use crate::config::Config;
use crate::http;
use crate::plugin::{DepType, Dependency, Plugin};
use crate::progress;

pub struct RequirementsPlugin {
    client: reqwest::Client,
}

impl RequirementsPlugin {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRequirement {
    name: String,
    spec_target: String,
    current_version: String,
}

fn strip_inline_comment(line: &str) -> (&str, &str) {
    match line.find('#') {
        Some(index) => (&line[..index], &line[index..]),
        None => (line, ""),
    }
}

fn parse_requirement_spec(spec: &str) -> Option<ParsedRequirement> {
    let trimmed = spec.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    if trimmed.starts_with('-') {
        return None;
    }

    if trimmed.contains(" @ ") {
        return None;
    }

    let requirement = trimmed.split(';').next().unwrap_or(trimmed).trim();
    if requirement.is_empty() {
        return None;
    }

    let version_start = requirement.find(['>', '<', '=', '!', '~']);
    match version_start {
        Some(pos) => {
            let spec_target = requirement[..pos].trim().to_string();
            let name = spec_target
                .split('[')
                .next()
                .unwrap_or(&spec_target)
                .trim()
                .to_string();
            if name.is_empty() {
                return None;
            }
            let current_version = requirement[pos..].trim().to_string();
            Some(ParsedRequirement {
                name,
                spec_target,
                current_version,
            })
        }
        None => {
            let spec_target = requirement.to_string();
            let name = spec_target
                .split('[')
                .next()
                .unwrap_or(&spec_target)
                .trim()
                .to_string();
            if name.is_empty() {
                return None;
            }
            Some(ParsedRequirement {
                name,
                spec_target,
                current_version: "*".to_string(),
            })
        }
    }
}

fn parse_requirement_line(line: &str) -> Option<ParsedRequirement> {
    let (content, _) = strip_inline_comment(line);
    parse_requirement_spec(content)
}

fn rewrite_requirement_line(line: &str, dep: &Dependency, preserve: bool) -> String {
    let Some(latest) = dep.latest_version.as_deref() else {
        return line.to_string();
    };

    let Some(parsed) = parse_requirement_line(line) else {
        return line.to_string();
    };

    if parsed.name != dep.name {
        return line.to_string();
    }

    let trimmed_start = line.trim_start();
    let leading_len = line.len() - trimmed_start.len();
    let leading = &line[..leading_len];

    let (content, comment) = strip_inline_comment(trimmed_start);
    let content_trimmed = content.trim();
    let marker = content_trimmed
        .find(';')
        .map(|index| &content_trimmed[index..])
        .unwrap_or("");

    let updated_spec = if preserve {
        let prefix: String = parsed
            .current_version
            .chars()
            .take_while(|c| !c.is_ascii_digit())
            .collect();
        format!("{}{prefix}{latest}", parsed.spec_target)
    } else {
        format!("{}=={latest}", parsed.spec_target)
    };

    let mut updated = format!("{leading}{updated_spec}");
    if !marker.is_empty() {
        updated.push_str(marker);
    }
    if !comment.is_empty() {
        if !updated.ends_with(' ') {
            updated.push(' ');
        }
        updated.push_str(comment.trim_start());
    }
    updated
}

fn parse_requirements(content: &str) -> Vec<Dependency> {
    content
        .lines()
        .filter_map(parse_requirement_line)
        .map(|parsed| Dependency::new(parsed.name, parsed.current_version, DepType::Normal))
        .collect()
}

async fn fetch_latest(client: &reqwest::Client, name: &str) -> Result<String> {
    let url = format!("https://pypi.org/pypi/{name}/json");
    let resp: PypiResponse = http::get_json_with_retries(|| client.get(&url)).await?;
    Ok(resp.info.version)
}

#[async_trait]
impl Plugin for RequirementsPlugin {
    fn name(&self) -> &str {
        "requirements"
    }

    fn file_name(&self) -> &str {
        "requirements.txt"
    }

    async fn list_dependencies(&self, dir: &Path) -> Result<Vec<Dependency>> {
        let path = dir.join(self.file_name());
        let content = std::fs::read_to_string(&path)?;
        Ok(parse_requirements(&content))
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
        let path = dir.join(self.file_name());
        let content = std::fs::read_to_string(&path)?;

        let updates = self.check_updates(dir, config).await?;
        let to_update: Vec<_> = updates
            .into_iter()
            .filter(|d| d.has_update())
            .filter(|d| dep_names.is_empty() || dep_names.contains(&d.name))
            .collect();

        let rewritten = content
            .lines()
            .map(|line| {
                let Some(parsed) = parse_requirement_line(line) else {
                    return line.to_string();
                };
                let Some(dep) = to_update.iter().find(|dep| dep.name == parsed.name) else {
                    return line.to_string();
                };
                rewrite_requirement_line(line, dep, config.preserve_range)
            })
            .collect::<Vec<_>>()
            .join("\n");

        let output = if content.ends_with('\n') {
            format!("{rewritten}\n")
        } else {
            rewritten
        };

        std::fs::write(&path, output)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_requirement_line, parse_requirements, rewrite_requirement_line};
    use crate::plugin::{DepType, Dependency};

    #[test]
    fn parse_requirement_line_skips_directives_and_comments() {
        assert!(parse_requirement_line("").is_none());
        assert!(parse_requirement_line("# comment").is_none());
        assert!(parse_requirement_line("-r base.txt").is_none());
        assert!(parse_requirement_line("pkg @ https://example.com/pkg.whl").is_none());
    }

    #[test]
    fn parse_requirement_line_supports_extras_markers_and_comments() {
        let parsed = parse_requirement_line(
            "requests[socks]>=2.31 ; python_version >= '3.10'  # keep comment",
        )
        .expect("failed to parse requirement");

        assert_eq!(parsed.name, "requests");
        assert_eq!(parsed.spec_target, "requests[socks]");
        assert_eq!(parsed.current_version, ">=2.31");
    }

    #[test]
    fn parse_requirements_keeps_unpinned_packages() {
        let deps = parse_requirements("rich\npytest==8.3.5\n");
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "rich");
        assert_eq!(deps[0].current_version, "*");
        assert_eq!(deps[1].name, "pytest");
        assert_eq!(deps[1].current_version, "==8.3.5");
    }

    #[test]
    fn rewrite_requirement_line_preserves_marker_and_comment() {
        let mut dep = Dependency::new(
            "requests".to_string(),
            ">=2.31".to_string(),
            DepType::Normal,
        );
        dep.latest_version = Some("2.32.3".to_string());

        let updated = rewrite_requirement_line(
            "  requests[socks]>=2.31 ; python_version >= '3.10'  # keep comment",
            &dep,
            true,
        );

        assert_eq!(
            updated,
            "  requests[socks]>=2.32.3; python_version >= '3.10' # keep comment"
        );
    }

    #[test]
    fn rewrite_requirement_line_can_pin_latest() {
        let mut dep = Dependency::new(
            "requests".to_string(),
            ">=2.31".to_string(),
            DepType::Normal,
        );
        dep.latest_version = Some("2.32.3".to_string());

        let updated = rewrite_requirement_line("requests[socks]>=2.31", &dep, false);
        assert_eq!(updated, "requests[socks]==2.32.3");
    }
}