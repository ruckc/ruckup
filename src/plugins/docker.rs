use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use serde::Deserialize;

use crate::config::Config;
use crate::http;
use crate::plugin::{DepType, Dependency, Plugin};
use crate::progress;

pub struct DockerPlugin {
    client: reqwest::Client,
}

impl DockerPlugin {
    pub fn new() -> Self {
        Self {
            client: http::default_client().expect("failed to build HTTP client"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedImage {
    name: String,
    current_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedOccurrence {
    name: String,
    current_version: String,
    replace_start: usize,
    replace_end: usize,
    dep_type: DepType,
}

#[derive(Deserialize)]
struct DockerHubTagsResponse {
    results: Vec<DockerHubTag>,
    next: Option<String>,
}

#[derive(Deserialize)]
struct DockerHubTag {
    name: String,
}

fn docker_manifest_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if is_dockerfile_name(name) || is_compose_name(name) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn is_dockerfile_name(name: &str) -> bool {
    name == "Dockerfile" || name.starts_with("Dockerfile.")
}

fn is_compose_name(name: &str) -> bool {
    matches!(
        name,
        "docker-compose.yml" | "docker-compose.yaml" | "compose.yml" | "compose.yaml"
    )
}

fn strip_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn parse_image_reference(value: &str) -> Option<ParsedImage> {
    let token = strip_quotes(value.trim());
    if token.is_empty() || token.contains('@') || token.contains("${") {
        return None;
    }

    let tag_pos = token.rfind(':')?;
    let last_slash = token.rfind('/');
    if last_slash.is_some_and(|slash| tag_pos < slash) {
        return None;
    }

    let name = token[..tag_pos].trim();
    let current_version = token[tag_pos + 1..].trim();
    if name.is_empty() || current_version.is_empty() {
        return None;
    }

    Some(ParsedImage {
        name: name.to_string(),
        current_version: current_version.to_string(),
    })
}

fn parse_dockerfile_occurrence(line: &str) -> Option<ParsedOccurrence> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let first = parts.next()?;
    if !first.eq_ignore_ascii_case("FROM") {
        return None;
    }

    let image_token = parts.find(|part| !part.starts_with("--"))?;
    let image = parse_image_reference(image_token)?;
    let leading = line.len() - trimmed.len();
    let start = leading + trimmed.find(image_token)?;
    let end = start + image_token.len();

    Some(ParsedOccurrence {
        name: image.name,
        current_version: image.current_version,
        replace_start: start,
        replace_end: end,
        dep_type: DepType::Build,
    })
}

fn parse_compose_occurrence(line: &str) -> Option<ParsedOccurrence> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }

    let remainder = trimmed.strip_prefix("image:")?.trim_start();
    if remainder.is_empty() {
        return None;
    }

    let token = if let Some(rest) = remainder.strip_prefix('"') {
        &remainder[1..1 + rest.find('"')?]
    } else if let Some(rest) = remainder.strip_prefix('\'') {
        &remainder[1..1 + rest.find('\'')?]
    } else {
        remainder
            .split_whitespace()
            .next()?
            .split('#')
            .next()?
            .trim()
    };

    let image = parse_image_reference(token)?;
    let leading = line.len() - trimmed.len();
    let start = leading + trimmed.find(token)?;
    let end = start + token.len();

    Some(ParsedOccurrence {
        name: image.name,
        current_version: image.current_version,
        replace_start: start,
        replace_end: end,
        dep_type: DepType::Normal,
    })
}

fn parse_line(line: &str, dep_type: DepType) -> Option<ParsedOccurrence> {
    match dep_type {
        DepType::Build => parse_dockerfile_occurrence(line),
        DepType::Normal => parse_compose_occurrence(line),
        _ => None,
    }
}

fn docker_hub_repo(name: &str) -> Option<String> {
    let first = name.split('/').next().unwrap_or(name);
    let repo = if first.contains('.') || first.contains(':') || first == "localhost" {
        if !matches!(
            first,
            "docker.io" | "index.docker.io" | "registry-1.docker.io"
        ) {
            return None;
        }
        name.split_once('/')?.1
    } else {
        name
    };

    Some(if repo.contains('/') {
        repo.to_string()
    } else {
        format!("library/{repo}")
    })
}

fn parse_version_tag(tag: &str) -> Option<(Vec<u64>, &str)> {
    let normalized = tag.strip_prefix('v').unwrap_or(tag);
    let prefix_len = normalized
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .count();
    if prefix_len == 0 {
        return None;
    }

    let prefix = &normalized[..prefix_len];
    let parts: Option<Vec<u64>> = prefix
        .split('.')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<u64>().ok())
        .collect();

    let parts = parts?;
    if parts.is_empty() {
        return None;
    }

    Some((parts, &normalized[prefix_len..]))
}

fn version_cmp(left: &[u64], right: &[u64]) -> Ordering {
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
        let left_part = *left.get(index).unwrap_or(&0);
        let right_part = *right.get(index).unwrap_or(&0);
        match left_part.cmp(&right_part) {
            Ordering::Equal => continue,
            ordering => return ordering,
        }
    }
    Ordering::Equal
}

fn select_newer_tag(current: &str, tags: &[String]) -> Option<String> {
    let (current_parts, current_suffix) = parse_version_tag(current)?;
    let mut best: Option<(&String, Vec<u64>)> = None;

    for tag in tags {
        if tag == current {
            continue;
        }
        let Some((parts, suffix)) = parse_version_tag(tag) else {
            continue;
        };
        if suffix != current_suffix || version_cmp(&parts, &current_parts) != Ordering::Greater {
            continue;
        }

        match &best {
            Some((_, best_parts)) if version_cmp(&parts, best_parts) != Ordering::Greater => {}
            _ => best = Some((tag, parts)),
        }
    }

    best.map(|(tag, _)| tag.clone())
}

fn rewrite_line(line: &str, dep: &Dependency, dep_type: DepType) -> String {
    let Some(occurrence) = parse_line(line, dep_type.clone()) else {
        return line.to_string();
    };
    if occurrence.name != dep.name || occurrence.current_version != dep.current_version {
        return line.to_string();
    }
    let Some(latest) = dep.latest_version.as_deref() else {
        return line.to_string();
    };

    format!(
        "{}{}:{}{}",
        &line[..occurrence.replace_start],
        occurrence.name,
        latest,
        &line[occurrence.replace_end..],
    )
}

async fn fetch_tags(client: &reqwest::Client, repo: &str) -> Result<Vec<String>> {
    let mut next = Some(format!(
        "https://hub.docker.com/v2/repositories/{repo}/tags?page_size=100"
    ));
    let mut tags = Vec::new();
    let mut pages = 0usize;

    while let Some(url) = next.take() {
        let response: DockerHubTagsResponse =
            http::get_json_with_retries(|| client.get(&url)).await?;
        tags.extend(response.results.into_iter().map(|tag| tag.name));
        next = response.next;
        pages += 1;
        if pages >= 5 {
            break;
        }
    }

    Ok(tags)
}

async fn fetch_latest_tag(client: &reqwest::Client, name: &str, current: &str) -> Result<String> {
    let repo = docker_hub_repo(name).ok_or_else(|| anyhow!("unsupported image registry"))?;
    let tags = fetch_tags(client, &repo).await?;
    Ok(select_newer_tag(current, &tags).unwrap_or_else(|| current.to_string()))
}

#[async_trait]
impl Plugin for DockerPlugin {
    fn name(&self) -> &str {
        "docker"
    }

    fn file_name(&self) -> &str {
        "Dockerfile* / docker-compose.yml"
    }

    fn detect(&self, dir: &Path) -> bool {
        docker_manifest_paths(dir)
            .map(|paths| !paths.is_empty())
            .unwrap_or(false)
    }

    fn display_name(&self, dir: &Path) -> String {
        match docker_manifest_paths(dir) {
            Ok(paths) if !paths.is_empty() => {
                let names = paths
                    .iter()
                    .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("docker ({names})")
            }
            _ => format!("{} ({})", self.name(), self.file_name()),
        }
    }

    async fn list_dependencies(&self, dir: &Path) -> Result<Vec<Dependency>> {
        let mut deps = Vec::new();

        for path in docker_manifest_paths(dir)? {
            let content = fs::read_to_string(&path)?;
            let dep_type = if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_dockerfile_name)
            {
                DepType::Build
            } else {
                DepType::Normal
            };

            for line in content.lines() {
                if let Some(occurrence) = parse_line(line, dep_type.clone()) {
                    deps.push(Dependency::new(
                        occurrence.name,
                        occurrence.current_version,
                        dep_type.clone(),
                    ));
                }
            }
        }

        Ok(deps)
    }

    async fn check_updates(&self, dir: &Path, config: &Config) -> Result<Vec<Dependency>> {
        let deps = self.list_dependencies(dir).await?;
        let client = &self.client;
        let concurrency = config.docker_concurrency;

        let pb = progress::check_progress_bar(deps.len() as u64);

        let results: Vec<Dependency> = stream::iter(deps)
            .map(|dep| async move {
                let mut dep = dep;

                if parse_version_tag(&dep.current_version).is_none() {
                    dep.satisfied = true;
                    dep.latest_version = Some(dep.current_version.clone());
                    return dep;
                }

                match fetch_latest_tag(client, &dep.name, &dep.current_version).await {
                    Ok(latest) => {
                        dep.satisfied = latest == dep.current_version;
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
        let updates = self.check_updates(dir, config).await?;
        let to_update: Vec<_> = updates
            .into_iter()
            .filter(|dep| dep.has_update())
            .filter(|dep| dep_names.is_empty() || dep_names.contains(&dep.name))
            .collect();

        for path in docker_manifest_paths(dir)? {
            let content = fs::read_to_string(&path)?;
            let dep_type = if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_dockerfile_name)
            {
                DepType::Build
            } else {
                DepType::Normal
            };

            let rewritten = content
                .lines()
                .map(|line| {
                    let Some(occurrence) = parse_line(line, dep_type.clone()) else {
                        return line.to_string();
                    };
                    let Some(dep) = to_update.iter().find(|dep| {
                        dep.name == occurrence.name
                            && dep.current_version == occurrence.current_version
                            && dep.dep_type == occurrence.dep_type
                    }) else {
                        return line.to_string();
                    };
                    rewrite_line(line, dep, dep_type.clone())
                })
                .collect::<Vec<_>>()
                .join("\n");

            let output = if content.ends_with('\n') {
                format!("{rewritten}\n")
            } else {
                rewritten
            };
            fs::write(&path, output)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::{
        DepType, docker_hub_repo, parse_compose_occurrence, parse_dockerfile_occurrence,
        parse_image_reference, select_newer_tag, version_cmp,
    };

    #[test]
    fn parse_image_reference_requires_tagged_images() {
        let parsed = parse_image_reference("node:20-alpine").expect("failed to parse image");
        assert_eq!(parsed.name, "node");
        assert_eq!(parsed.current_version, "20-alpine");
        assert!(parse_image_reference("python").is_none());
        assert!(parse_image_reference("python@sha256:deadbeef").is_none());
    }

    #[test]
    fn parse_dockerfile_occurrence_handles_platform_and_alias() {
        let parsed = parse_dockerfile_occurrence(
            "FROM --platform=$BUILDPLATFORM rust:1.86.0-alpine AS builder",
        )
        .expect("failed to parse Dockerfile line");
        assert_eq!(parsed.name, "rust");
        assert_eq!(parsed.current_version, "1.86.0-alpine");
        assert_eq!(parsed.dep_type, DepType::Build);
    }

    #[test]
    fn parse_compose_occurrence_handles_quotes_and_comments() {
        let parsed = parse_compose_occurrence("    image: \"postgres:16.4\" # db image")
            .expect("failed to parse compose image line");
        assert_eq!(parsed.name, "postgres");
        assert_eq!(parsed.current_version, "16.4");
        assert_eq!(parsed.dep_type, DepType::Normal);
    }

    #[test]
    fn docker_hub_repo_normalizes_official_images() {
        assert_eq!(docker_hub_repo("python").as_deref(), Some("library/python"));
        assert_eq!(
            docker_hub_repo("docker.io/library/python").as_deref(),
            Some("library/python")
        );
        assert!(docker_hub_repo("ghcr.io/acme/app").is_none());
    }

    #[test]
    fn tag_selection_preserves_suffix_family() {
        let tags = vec![
            "19-alpine".to_string(),
            "20-alpine".to_string(),
            "20.1-alpine".to_string(),
            "21-bullseye".to_string(),
        ];
        assert_eq!(
            select_newer_tag("18-alpine", &tags).as_deref(),
            Some("20.1-alpine")
        );
        assert!(select_newer_tag("latest", &tags).is_none());
    }

    #[test]
    fn version_cmp_pads_missing_parts() {
        assert_eq!(version_cmp(&[1, 0], &[1]), Ordering::Equal);
        assert_eq!(version_cmp(&[1, 2], &[1, 1, 9]), Ordering::Greater);
    }
}
