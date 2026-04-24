use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use serde::Deserialize;

use crate::config::Config;
use crate::http;
use crate::plugin::{DepType, Dependency, Plugin};
use crate::progress;

fn replacement_ref(current: &str, latest: &str) -> Option<String> {
    if current == latest {
        return None;
    }

    if matches!(current, "main" | "master" | "stable" | "beta" | "nightly") {
        return None;
    }

    let suffix = current.rsplit('/').next().unwrap_or(current);
    if suffix != current && action_ref_matches(current, latest) {
        return None;
    }

    Some(latest.to_string())
}

pub struct GithubActionsPlugin {
    client: reqwest::Client,
}

impl GithubActionsPlugin {
    pub fn new() -> Self {
        Self {
            client: http::github_client().expect("failed to build HTTP client"),
        }
    }
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

#[derive(Deserialize)]
struct GithubTag {
    name: String,
}

fn workflows_dir(dir: &Path) -> PathBuf {
    dir.join(".github").join("workflows")
}

fn workflow_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let workflows = workflows_dir(dir);
    if !workflows.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(&workflows)
        .with_context(|| format!("failed to read {}", workflows.display()))?
    {
        let path = entry?.path();
        let ext = path.extension().and_then(|ext| ext.to_str());
        if matches!(ext, Some("yml") | Some("yaml")) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn parse_uses_value(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || !trimmed.starts_with("uses:") {
        return None;
    }

    let raw = trimmed[5..].trim();
    let value = raw
        .split_whitespace()
        .next()?
        .trim_matches('"')
        .trim_matches('\'');

    if value.starts_with("./") || value.starts_with("docker://") || value.starts_with("${{") {
        return None;
    }

    let (name, version) = value.rsplit_once('@')?;
    if !name.contains('/') || version.is_empty() {
        return None;
    }

    Some((name.to_string(), version.to_string()))
}

fn action_repo(name: &str) -> Option<&str> {
    let mut parts = name.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    Some(&name[..owner.len() + repo.len() + 1])
}

fn version_prefix(value: &str) -> Option<Vec<u64>> {
    let trimmed = value.trim_start_matches('v');
    let prefix = trimmed
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect::<String>();
    if prefix.is_empty() {
        return None;
    }

    let numbers: Option<Vec<u64>> = prefix
        .split('.')
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<u64>().ok())
        .collect();

    match numbers {
        Some(parts) if !parts.is_empty() => Some(parts),
        _ => None,
    }
}

fn action_ref_matches(current: &str, latest: &str) -> bool {
    if current == latest {
        return true;
    }

    if matches!(current, "main" | "master" | "stable" | "beta" | "nightly") {
        return true;
    }

    let current = current.rsplit('/').next().unwrap_or(current);

    let Some(current_parts) = version_prefix(current) else {
        return false;
    };
    let Some(latest_parts) = version_prefix(latest) else {
        return false;
    };

    current_parts.len() <= latest_parts.len()
        && current_parts
            .iter()
            .zip(latest_parts.iter())
            .all(|(current, latest)| current == latest)
}

async fn fetch_latest(client: &reqwest::Client, repo: &str) -> Result<String> {
    let release_url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let release = client
        .get(&release_url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;

    if release.status().is_success() {
        let release: GithubRelease = release.json().await?;
        return Ok(release.tag_name);
    }

    let tag_url = format!("https://api.github.com/repos/{repo}/tags?per_page=1");
    let response = client
        .get(&tag_url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;

    if response.status().is_success() {
        let tags: Vec<GithubTag> = response.json().await?;
        if let Some(tag) = tags.into_iter().next() {
            return Ok(tag.name);
        }
    }

    fetch_latest_git_tag(repo)
}

fn fetch_latest_git_tag(repo: &str) -> Result<String> {
    let remote = format!("https://github.com/{repo}");
    let output = Command::new("git")
        .args([
            "ls-remote",
            "--tags",
            "--refs",
            "--sort=-version:refname",
            &remote,
        ])
        .output()
        .with_context(|| format!("failed to run git ls-remote for {repo}"))?;

    if !output.status.success() {
        bail!(
            "git ls-remote failed for {repo}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("invalid utf-8 from git ls-remote for {repo}"))?;
    let line = stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("no published tags found"))?;
    let tag = line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.strip_prefix("refs/tags/"))
        .ok_or_else(|| anyhow::anyhow!("failed to parse tag output"))?;

    Ok(tag.to_string())
}

fn rewrite_uses_line(line: &str, updates: &std::collections::HashMap<&str, &str>) -> String {
    let Some(uses_idx) = line.find("uses:") else {
        return line.to_string();
    };

    let value_start = uses_idx + "uses:".len();
    let prefix = &line[..value_start];
    let remainder = &line[value_start..];
    let trimmed = remainder.trim_start();
    let whitespace_len = remainder.len() - trimmed.len();
    let whitespace = &remainder[..whitespace_len];

    let quote = match trimmed.chars().next() {
        Some('"') => Some('"'),
        Some('\'') => Some('\''),
        _ => None,
    };
    let token_start = usize::from(quote.is_some());
    let token_body = &trimmed[token_start..];
    let token_end = token_body
        .find(|ch: char| ch.is_whitespace() || ch == '#' || ch == '"' || ch == '\'')
        .unwrap_or(token_body.len());
    let token = &token_body[..token_end];

    let Some((name, current_ref)) = parse_uses_value(&format!("uses: {token}")) else {
        return line.to_string();
    };
    let Some(latest_ref) = updates.get(name.as_str()) else {
        return line.to_string();
    };

    let updated_token = format!("{}@{}", name, latest_ref);
    let mut rebuilt = String::with_capacity(line.len() + latest_ref.len());
    rebuilt.push_str(prefix);
    rebuilt.push_str(whitespace);
    if let Some(quote) = quote {
        rebuilt.push(quote);
    }
    rebuilt.push_str(&updated_token);
    if let Some(quote) = quote {
        rebuilt.push(quote);
    }
    rebuilt.push_str(&token_body[token_end..]);

    if token == format!("{}@{}", name, current_ref) {
        rebuilt
    } else {
        line.to_string()
    }
}

#[async_trait]
impl Plugin for GithubActionsPlugin {
    fn name(&self) -> &str {
        "github-actions"
    }

    fn file_name(&self) -> &str {
        ".github/workflows"
    }

    fn detect(&self, dir: &Path) -> bool {
        workflow_files(dir)
            .map(|files| !files.is_empty())
            .unwrap_or(false)
    }

    fn display_name(&self, _dir: &Path) -> String {
        "GitHub Actions (.github/workflows)".to_string()
    }

    async fn list_dependencies(&self, dir: &Path) -> Result<Vec<Dependency>> {
        let files = workflow_files(dir)?;
        let mut seen = BTreeSet::new();
        let mut deps = Vec::new();

        for file in files {
            let content = fs::read_to_string(&file)
                .with_context(|| format!("failed to read {}", file.display()))?;

            for line in content.lines() {
                if let Some((name, version)) = parse_uses_value(line)
                    && seen.insert((name.clone(), version.clone()))
                {
                    deps.push(Dependency::new(name, version, DepType::Build));
                }
            }
        }

        Ok(deps)
    }

    async fn check_updates(&self, dir: &Path, config: &Config) -> Result<Vec<Dependency>> {
        let deps = self.list_dependencies(dir).await?;
        let client = &self.client;
        let concurrency = config.github_actions_concurrency;

        let pb = progress::check_progress_bar(deps.len() as u64);

        let results: Vec<Dependency> = stream::iter(deps)
            .map(|dep| async move {
                let mut dep = dep;
                match action_repo(&dep.name) {
                    Some(repo) => match fetch_latest(client, repo).await {
                        Ok(latest) => {
                            dep.satisfied = action_ref_matches(&dep.current_version, &latest);
                            dep.latest_version = Some(latest);
                        }
                        Err(err) => {
                            dep.check_failed = true;
                            eprintln!("  warning: failed to check {}: {err}", dep.name)
                        }
                    },
                    None => {
                        dep.check_failed = true;
                        eprintln!("  warning: unsupported action reference {}", dep.name)
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
        let to_update: std::collections::HashMap<String, String> = updates
            .into_iter()
            .filter(|dep| dep.has_update())
            .filter(|dep| dep_names.is_empty() || dep_names.contains(&dep.name))
            .filter_map(|dep| {
                let latest = dep.latest_version.as_deref()?;
                let replacement = replacement_ref(&dep.current_version, latest)?;
                Some((dep.name, replacement))
            })
            .collect();

        if to_update.is_empty() {
            return Ok(());
        }

        let borrowed_updates: std::collections::HashMap<&str, &str> = to_update
            .iter()
            .map(|(name, latest)| (name.as_str(), latest.as_str()))
            .collect();

        for file in workflow_files(dir)? {
            let content = fs::read_to_string(&file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            let updated = content
                .lines()
                .map(|line| rewrite_uses_line(line, &borrowed_updates))
                .collect::<Vec<_>>()
                .join("\n");

            if updated != content {
                let mut updated = updated;
                if content.ends_with('\n') {
                    updated.push('\n');
                }
                fs::write(&file, updated)
                    .with_context(|| format!("failed to write {}", file.display()))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{action_ref_matches, parse_uses_value};

    #[test]
    fn parse_uses_value_extracts_repo_and_ref() {
        let parsed = parse_uses_value("uses: actions/checkout@v4").expect("expected parsed uses");
        assert_eq!(parsed.0, "actions/checkout");
        assert_eq!(parsed.1, "v4");
    }

    #[test]
    fn parse_uses_value_ignores_local_actions() {
        assert!(parse_uses_value("uses: ./action").is_none());
        assert!(parse_uses_value("uses: docker://alpine:3.20").is_none());
    }

    #[test]
    fn action_ref_matches_major_prefix() {
        assert!(action_ref_matches("v4", "v4.1.0"));
        assert!(!action_ref_matches("v3", "v4.1.0"));
    }
}
