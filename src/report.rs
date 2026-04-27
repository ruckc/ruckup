use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use lopdf::{Document, Object, Stream, content::Content, content::Operation, dictionary};
use serde::Deserialize;
use tokio::sync::OnceCell;

use crate::http;
use crate::plugin::Dependency;

#[derive(Clone, Debug)]
pub enum ReportFormat {
    Text,
    Markdown,
    Html,
    Pdf,
}

impl ReportFormat {
    fn ext(&self) -> &'static str {
        match self {
            Self::Text => "txt",
            Self::Markdown => "md",
            Self::Html => "html",
            Self::Pdf => "pdf",
        }
    }

    fn default_name(&self) -> &'static str {
        match self {
            Self::Text => "ruckup-report.txt",
            Self::Markdown => "ruckup-report.md",
            Self::Html => "ruckup-report.html",
            Self::Pdf => "ruckup-report.pdf",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinkRef {
    pub label: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct SupplyChainDelta {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub note: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DependencyReport {
    pub name: String,
    pub current_version: String,
    pub latest_version: String,
    pub semver_impact: String,
    pub diff_links: Vec<LinkRef>,
    pub changelog_links: Vec<LinkRef>,
    pub security_links: Vec<LinkRef>,
    pub security_findings: Vec<String>,
    pub supply_chain: SupplyChainDelta,
}

#[derive(Clone, Debug)]
pub struct PluginReport {
    pub plugin: String,
    pub security_findings: Vec<String>,
    pub dependencies: Vec<DependencyReport>,
}

#[derive(Clone, Debug)]
pub struct UpgradeReport {
    pub generated_at_utc: String,
    pub plugins: Vec<PluginReport>,
}

pub async fn build_dependency_reports(
    plugin_name: &str,
    deps: Vec<Dependency>,
) -> Vec<DependencyReport> {
    let plugin_name = plugin_name.to_string();
    stream::iter(deps)
        .map(|dep| {
            let plugin_name = plugin_name.clone();
            async move { analyze_dependency(&plugin_name, &dep).await }
        })
        .buffer_unordered(8)
        .collect()
        .await
}

pub async fn plugin_security_findings(plugin_name: &str) -> Vec<String> {
    if plugin_name.eq_ignore_ascii_case("cargo") {
        return cargo_workspace_archived_findings().await;
    }
    Vec::new()
}

pub fn write_reports(
    report: &UpgradeReport,
    formats: &[ReportFormat],
    output: &str,
) -> Result<Vec<PathBuf>> {
    let targets = resolve_output_paths(formats, output)?;
    for (format, path) in &targets {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        match format {
            ReportFormat::Text => {
                std::fs::write(path, render_text(report))
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
            ReportFormat::Markdown => {
                std::fs::write(path, render_markdown(report))
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
            ReportFormat::Html => {
                std::fs::write(path, render_html(report))
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
            ReportFormat::Pdf => {
                write_pdf(path, &render_text(report))?;
            }
        }
    }

    Ok(targets.into_iter().map(|(_, p)| p).collect())
}

pub fn open_best_effort(paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }

    let preferred = paths
        .iter()
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("html"))
        .or_else(|| paths.first());

    if let Some(path) = preferred
        && let Ok(abs) = path.canonicalize()
    {
        let target = format!("file://{}", abs.to_string_lossy());
        if webbrowser::open(&target).is_ok() {
            return true;
        }

        if cfg!(target_os = "linux") {
            return std::process::Command::new("xdg-open")
                .arg(&abs)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        }

        if cfg!(target_os = "macos") {
            return std::process::Command::new("open")
                .arg(&abs)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        }

        if cfg!(target_os = "windows") {
            return std::process::Command::new("cmd")
                .arg("/C")
                .arg("start")
                .arg("")
                .arg(&abs)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        }
    }

    false
}

async fn analyze_dependency(plugin_name: &str, dep: &Dependency) -> DependencyReport {
    let latest = dep
        .latest_version
        .clone()
        .unwrap_or_else(|| dep.current_version.clone());
    let plugin_lower = plugin_name.to_ascii_lowercase();

    let semver_impact = classify_semver_change(&dep.current_version, &latest);

    let mut diff_links = Vec::new();
    let mut changelog_links = Vec::new();
    let mut security_links = Vec::new();
    let mut security_findings = Vec::new();
    let mut supply_chain = SupplyChainDelta {
        added: Vec::new(),
        removed: Vec::new(),
        changed: Vec::new(),
        note: Some("Supply-chain delta unavailable for this ecosystem/dependency.".to_string()),
    };

    match plugin_lower.as_str() {
        "cargo" => {
            let (repo, homepage) = cargo_repository(&dep.name).await;
            if let Some(url) = repo.clone() {
                if let Some(link) = github_compare_link(&url, &dep.current_version, &latest).await {
                    diff_links.push(LinkRef {
                        label: "SCM diff".to_string(),
                        url: link,
                    });
                }
                changelog_links.push(LinkRef {
                    label: "Releases".to_string(),
                    url: format!("{}/releases", trim_trailing_slash(&url)),
                });
            }
            diff_links.push(LinkRef {
                label: "Crates.io".to_string(),
                url: format!("https://crates.io/crates/{}", dep.name),
            });
            if let Some(home) = homepage {
                changelog_links.push(LinkRef {
                    label: "Homepage".to_string(),
                    url: home,
                });
            }
            security_links = vec![
                LinkRef {
                    label: "deps.dev".to_string(),
                    url: format!("https://deps.dev/cargo/{}", dep.name),
                },
                LinkRef {
                    label: "OSV".to_string(),
                    url: format!("https://osv.dev/list?ecosystem=crates.io&q={}", dep.name),
                },
                LinkRef {
                    label: "RustSec advisories".to_string(),
                    url: "https://rustsec.org/advisories/".to_string(),
                },
            ];

            security_findings = cargo_workspace_archived_findings().await;

            if let (Some(cur), Some(new)) = (
                extract_version_core(&dep.current_version),
                extract_version_core(&latest),
            ) {
                let current_deps = cargo_version_dependencies(&dep.name, &cur).await;
                let latest_deps = cargo_version_dependencies(&dep.name, &new).await;
                supply_chain = diff_dependency_maps(&current_deps, &latest_deps);
            }
        }
        "npm" => {
            let meta = npm_package_meta(&dep.name).await;
            if let Some(repo) = meta.repository.clone() {
                if let Some(link) = github_compare_link(&repo, &dep.current_version, &latest).await
                {
                    diff_links.push(LinkRef {
                        label: "SCM diff".to_string(),
                        url: link,
                    });
                }
                changelog_links.push(LinkRef {
                    label: "Releases".to_string(),
                    url: format!("{}/releases", trim_trailing_slash(&repo)),
                });
            }
            diff_links.push(LinkRef {
                label: "npm package".to_string(),
                url: format!("https://www.npmjs.com/package/{}", dep.name),
            });
            if let Some(home) = meta.homepage {
                changelog_links.push(LinkRef {
                    label: "Homepage".to_string(),
                    url: home,
                });
            }
            security_links = vec![
                LinkRef {
                    label: "deps.dev".to_string(),
                    url: format!("https://deps.dev/npm/{}", dep.name),
                },
                LinkRef {
                    label: "OSV".to_string(),
                    url: format!("https://osv.dev/list?ecosystem=npm&q={}", dep.name),
                },
                LinkRef {
                    label: "Snyk".to_string(),
                    url: format!("https://security.snyk.io/package/npm/{}", dep.name),
                },
                LinkRef {
                    label: "Socket".to_string(),
                    url: format!("https://socket.dev/npm/package/{}", dep.name),
                },
            ];

            if let (Some(cur), Some(new)) = (
                extract_version_core(&dep.current_version),
                extract_version_core(&latest),
            ) {
                let current_deps = npm_version_dependencies(&dep.name, &cur).await;
                let latest_deps = npm_version_dependencies(&dep.name, &new).await;
                supply_chain = diff_dependency_maps(&current_deps, &latest_deps);
            }
        }
        "pyproject" | "requirements" => {
            let meta = pypi_project_meta(&dep.name).await;
            if let Some(repo) = meta.repository.clone() {
                if let Some(link) = github_compare_link(&repo, &dep.current_version, &latest).await
                {
                    diff_links.push(LinkRef {
                        label: "SCM diff".to_string(),
                        url: link,
                    });
                }
                changelog_links.push(LinkRef {
                    label: "Releases".to_string(),
                    url: format!("{}/releases", trim_trailing_slash(&repo)),
                });
            }

            diff_links.push(LinkRef {
                label: "PyPI package".to_string(),
                url: format!("https://pypi.org/project/{}/", dep.name),
            });
            if let Some(changelog) = meta.changelog {
                changelog_links.push(LinkRef {
                    label: "Changelog".to_string(),
                    url: changelog,
                });
            }

            security_links = vec![
                LinkRef {
                    label: "deps.dev".to_string(),
                    url: format!("https://deps.dev/pypi/{}", dep.name),
                },
                LinkRef {
                    label: "OSV".to_string(),
                    url: format!("https://osv.dev/list?ecosystem=PyPI&q={}", dep.name),
                },
                LinkRef {
                    label: "Snyk".to_string(),
                    url: format!("https://security.snyk.io/package/pip/{}", dep.name),
                },
            ];

            if let (Some(cur), Some(new)) = (
                extract_version_core(&dep.current_version),
                extract_version_core(&latest),
            ) {
                let current_deps = pypi_version_dependencies(&dep.name, &cur).await;
                let latest_deps = pypi_version_dependencies(&dep.name, &new).await;
                supply_chain = diff_dependency_maps(&current_deps, &latest_deps);
            }
        }
        "github-actions" => {
            let repo = dep.name.split('/').take(2).collect::<Vec<_>>().join("/");
            if repo.contains('/') {
                let repo_url = format!("https://github.com/{repo}");
                if let Some(link) =
                    github_compare_link(&repo_url, &dep.current_version, &latest).await
                {
                    diff_links.push(LinkRef {
                        label: "SCM diff".to_string(),
                        url: link,
                    });
                }
                changelog_links.push(LinkRef {
                    label: "Releases".to_string(),
                    url: format!("https://github.com/{repo}/releases"),
                });
            }
            security_links = vec![
                LinkRef {
                    label: "GitHub Security".to_string(),
                    url: format!("https://github.com/{repo}/security"),
                },
                LinkRef {
                    label: "Dependabot alerts".to_string(),
                    url: "https://docs.github.com/en/code-security/dependabot".to_string(),
                },
            ];
            supply_chain.note = Some(
                "Transitive dependency graph is not directly available for GitHub Actions refs."
                    .to_string(),
            );
        }
        "docker" => {
            let image = dep.name.trim();
            let hub = if image.contains('/') {
                format!("https://hub.docker.com/r/{image}")
            } else {
                format!("https://hub.docker.com/_/{image}")
            };
            diff_links.push(LinkRef {
                label: "Docker Hub tags".to_string(),
                url: hub,
            });
            security_links = vec![
                LinkRef {
                    label: "Docker Scout".to_string(),
                    url: "https://docs.docker.com/scout/".to_string(),
                },
                LinkRef {
                    label: "Snyk container DB".to_string(),
                    url: "https://security.snyk.io/vuln/docker".to_string(),
                },
            ];
            supply_chain.note = Some("Container transitive packages vary by image build; no deterministic graph from tag metadata.".to_string());
        }
        _ => {}
    }

    let supply_chain = normalize_supply_chain(supply_chain);

    DependencyReport {
        name: dep.name.clone(),
        current_version: dep.current_version.clone(),
        latest_version: latest,
        semver_impact,
        diff_links,
        changelog_links,
        security_links,
        security_findings,
        supply_chain,
    }
}

fn normalize_supply_chain(mut delta: SupplyChainDelta) -> SupplyChainDelta {
    if delta.added.is_empty()
        && delta.removed.is_empty()
        && delta.changed.is_empty()
        && delta.note.is_none()
    {
        delta.note = Some("No transitive dependency changes detected.".to_string());
    }
    delta
}

fn resolve_output_paths(
    formats: &[ReportFormat],
    output: &str,
) -> Result<Vec<(ReportFormat, PathBuf)>> {
    let mut unique_formats = Vec::new();
    for format in formats {
        if !unique_formats
            .iter()
            .any(|f: &ReportFormat| std::mem::discriminant(f) == std::mem::discriminant(format))
        {
            unique_formats.push(format.clone());
        }
    }

    let output_path = PathBuf::from(output);
    let treat_as_dir = output.ends_with('/') || output_path.is_dir();

    if treat_as_dir {
        std::fs::create_dir_all(&output_path)
            .with_context(|| format!("failed to create {}", output_path.display()))?;
        return Ok(unique_formats
            .into_iter()
            .map(|fmt| {
                let file = output_path.join(fmt.default_name());
                (fmt, file)
            })
            .collect());
    }

    if unique_formats.len() == 1 {
        let fmt = unique_formats.remove(0);
        let path = if output_path.extension().is_some() {
            output_path
        } else {
            output_path.with_extension(fmt.ext())
        };
        return Ok(vec![(fmt, path)]);
    }

    Ok(unique_formats
        .into_iter()
        .map(|fmt| {
            let path = output_path.with_extension(fmt.ext());
            (fmt, path)
        })
        .collect())
}

fn render_text(report: &UpgradeReport) -> String {
    let mut out = String::new();
    out.push_str("ruckup upgrade intelligence report\n");
    out.push_str(&format!("generated at: {}\n\n", report.generated_at_utc));

    if report.plugins.is_empty() {
        out.push_str("No upgrade candidates found.\n");
        return out;
    }

    for plugin in &report.plugins {
        out.push_str(&format!("== {} ==\n", plugin.plugin));
        if !plugin.security_findings.is_empty() {
            out.push_str("  Security findings (full dependency tree):\n");
            for finding in &plugin.security_findings {
                out.push_str(&format!("    - {}\n", finding));
            }
        }
        for dep in &plugin.dependencies {
            out.push_str(&format!(
                "- {}: {} -> {} ({})\n",
                dep.name, dep.current_version, dep.latest_version, dep.semver_impact
            ));
            write_text_link_group(&mut out, "Diff", &dep.diff_links);
            write_text_link_group(&mut out, "Changelog", &dep.changelog_links);
            write_text_link_group(&mut out, "Security", &dep.security_links);
            write_text_findings(&mut out, &dep.security_findings);
            write_supply_chain_text(&mut out, &dep.supply_chain);
            out.push('\n');
        }
    }

    out
}

fn write_text_findings(out: &mut String, findings: &[String]) {
    if findings.is_empty() {
        return;
    }
    out.push_str("  Security findings:\n");
    for finding in findings {
        out.push_str(&format!("    - {}\n", finding));
    }
}

fn write_text_link_group(out: &mut String, title: &str, links: &[LinkRef]) {
    if links.is_empty() {
        return;
    }
    out.push_str(&format!("  {} links:\n", title));
    for link in links {
        out.push_str(&format!("    - {}: {}\n", link.label, link.url));
    }
}

fn write_supply_chain_text(out: &mut String, delta: &SupplyChainDelta) {
    out.push_str("  Supply chain:\n");
    if let Some(note) = &delta.note {
        out.push_str(&format!("    - {}\n", note));
        return;
    }
    if delta.added.is_empty() && delta.removed.is_empty() && delta.changed.is_empty() {
        out.push_str("    - no transitive dependency changes detected\n");
        return;
    }
    for item in &delta.added {
        out.push_str(&format!("    - added: {}\n", item));
    }
    for item in &delta.removed {
        out.push_str(&format!("    - removed: {}\n", item));
    }
    for item in &delta.changed {
        out.push_str(&format!("    - changed: {}\n", item));
    }
}

fn render_markdown(report: &UpgradeReport) -> String {
    let mut out = String::new();
    out.push_str("# ruckup Upgrade Intelligence Report\n\n");
    out.push_str(&format!("Generated at: {}\n\n", report.generated_at_utc));

    if report.plugins.is_empty() {
        out.push_str("No upgrade candidates found.\n");
        return out;
    }

    for plugin in &report.plugins {
        out.push_str(&format!("## {}\n\n", plugin.plugin));
        if !plugin.security_findings.is_empty() {
            out.push_str("- Security findings (full dependency tree):\n");
            for finding in &plugin.security_findings {
                out.push_str(&format!("  - {}\n", finding));
            }
            out.push('\n');
        }
        for dep in &plugin.dependencies {
            out.push_str(&format!(
                "### {}\n\n- Current: `{}`\n- Latest: `{}`\n- Semver impact: **{}**\n",
                dep.name, dep.current_version, dep.latest_version, dep.semver_impact
            ));

            write_markdown_links(&mut out, "Diff links", &dep.diff_links);
            write_markdown_links(&mut out, "Changelog links", &dep.changelog_links);
            write_markdown_links(&mut out, "Security links", &dep.security_links);
            write_markdown_findings(&mut out, &dep.security_findings);
            write_markdown_supply_chain(&mut out, &dep.supply_chain);
            out.push('\n');
        }
    }

    out
}

fn write_markdown_findings(out: &mut String, findings: &[String]) {
    if findings.is_empty() {
        return;
    }
    out.push_str("\n- Security findings:\n");
    for finding in findings {
        out.push_str(&format!("  - {}\n", finding));
    }
}

fn write_markdown_links(out: &mut String, title: &str, links: &[LinkRef]) {
    if links.is_empty() {
        return;
    }
    out.push_str(&format!("\n- {}:\n", title));
    for link in links {
        out.push_str(&format!("  - [{}]({})\n", link.label, link.url));
    }
}

fn write_markdown_supply_chain(out: &mut String, delta: &SupplyChainDelta) {
    out.push_str("\n- Supply-chain delta:\n");
    if let Some(note) = &delta.note {
        out.push_str(&format!("  - {}\n", note));
        return;
    }

    if delta.added.is_empty() && delta.removed.is_empty() && delta.changed.is_empty() {
        out.push_str("  - No transitive dependency changes detected\n");
        return;
    }

    for item in &delta.added {
        out.push_str(&format!("  - Added: {}\n", item));
    }
    for item in &delta.removed {
        out.push_str(&format!("  - Removed: {}\n", item));
    }
    for item in &delta.changed {
        out.push_str(&format!("  - Changed: {}\n", item));
    }
}

fn render_html(report: &UpgradeReport) -> String {
    let mut out = String::new();
    out.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    out.push_str("<title>ruckup upgrade report</title><style>");
    out.push_str("body{font-family:Georgia,serif;margin:0;background:linear-gradient(180deg,#f8f5ef,#e9edf4);color:#111;}main{max-width:960px;margin:0 auto;padding:2rem 1rem;}h1{font-size:2rem;margin:.2rem 0 1rem;}h2{margin-top:2rem;border-bottom:1px solid #bbb;padding-bottom:.35rem;}article{background:#fff;border-radius:12px;padding:1rem 1.2rem;margin:1rem 0;box-shadow:0 6px 18px rgba(0,0,0,.08);}small{color:#555;}ul{margin:.35rem 0 .75rem 1.25rem;}li{margin:.15rem 0;}code{background:#f0f2f6;padding:.1rem .3rem;border-radius:4px;}a{color:#0f4c81;text-decoration:none;}a:hover{text-decoration:underline;} .pill{display:inline-block;padding:.1rem .55rem;border-radius:999px;background:#113f67;color:#fff;font-size:.85rem;}</style></head><body><main>");
    out.push_str("<h1>ruckup upgrade intelligence report</h1>");
    out.push_str(&format!(
        "<small>Generated at: {}</small>",
        escape_html(&report.generated_at_utc)
    ));

    if report.plugins.is_empty() {
        out.push_str("<p>No upgrade candidates found.</p>");
    }

    for plugin in &report.plugins {
        out.push_str(&format!("<h2>{}</h2>", escape_html(&plugin.plugin)));
        if !plugin.security_findings.is_empty() {
            out.push_str("<article>");
            out.push_str("<h3>Security findings (full dependency tree)</h3><ul>");
            for finding in &plugin.security_findings {
                out.push_str(&format!("<li>{}</li>", escape_html(finding)));
            }
            out.push_str("</ul></article>");
        }
        for dep in &plugin.dependencies {
            out.push_str("<article>");
            out.push_str(&format!("<h3>{}</h3>", escape_html(&dep.name)));
            out.push_str(&format!(
                "<p><code>{}</code> &rarr; <code>{}</code> <span class=\"pill\">{}</span></p>",
                escape_html(&dep.current_version),
                escape_html(&dep.latest_version),
                escape_html(&dep.semver_impact)
            ));
            write_html_links(&mut out, "Diff links", &dep.diff_links);
            write_html_links(&mut out, "Changelog links", &dep.changelog_links);
            write_html_links(&mut out, "Security links", &dep.security_links);
            write_html_findings(&mut out, &dep.security_findings);
            write_html_supply_chain(&mut out, &dep.supply_chain);
            out.push_str("</article>");
        }
    }

    out.push_str("</main></body></html>");
    out
}

fn write_html_findings(out: &mut String, findings: &[String]) {
    if findings.is_empty() {
        return;
    }
    out.push_str("<strong>Security findings</strong><ul>");
    for finding in findings {
        out.push_str(&format!("<li>{}</li>", escape_html(finding)));
    }
    out.push_str("</ul>");
}

fn write_html_links(out: &mut String, title: &str, links: &[LinkRef]) {
    if links.is_empty() {
        return;
    }
    out.push_str(&format!("<strong>{}</strong><ul>", escape_html(title)));
    for link in links {
        out.push_str(&format!(
            "<li><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a></li>",
            escape_html(&link.url),
            escape_html(&link.label)
        ));
    }
    out.push_str("</ul>");
}

fn write_html_supply_chain(out: &mut String, delta: &SupplyChainDelta) {
    out.push_str("<strong>Supply-chain delta</strong><ul>");
    if let Some(note) = &delta.note {
        out.push_str(&format!("<li>{}</li>", escape_html(note)));
        out.push_str("</ul>");
        return;
    }
    if delta.added.is_empty() && delta.removed.is_empty() && delta.changed.is_empty() {
        out.push_str("<li>No transitive dependency changes detected</li></ul>");
        return;
    }
    for item in &delta.added {
        out.push_str(&format!("<li>Added: {}</li>", escape_html(item)));
    }
    for item in &delta.removed {
        out.push_str(&format!("<li>Removed: {}</li>", escape_html(item)));
    }
    for item in &delta.changed {
        out.push_str(&format!("<li>Changed: {}</li>", escape_html(item)));
    }
    out.push_str("</ul>");
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn write_pdf(path: &Path, text: &str) -> Result<()> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });

    let wrapped = wrap_text_lines(text, 110);
    let lines_per_page = 55usize;
    let mut page_ids = Vec::new();

    for chunk in wrapped.chunks(lines_per_page) {
        let mut ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec![Object::Name(b"F1".to_vec()), Object::Real(10.0)]),
            Operation::new("TL", vec![Object::Real(14.0)]),
            Operation::new("Td", vec![Object::Real(36.0), Object::Real(806.0)]),
        ];

        for (idx, raw_line) in chunk.iter().enumerate() {
            if idx > 0 {
                ops.push(Operation::new("T*", vec![]));
            }
            ops.push(Operation::new(
                "Tj",
                vec![Object::string_literal(raw_line.as_str())],
            ));
        }

        ops.push(Operation::new("ET", vec![]));
        let content = Content { operations: ops };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode()?));

        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Contents" => content_id,
            "Resources" => dictionary! {
                "Font" => dictionary! {
                    "F1" => font_id,
                },
            },
        });
        page_ids.push(page_id);
    }

    if page_ids.is_empty() {
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Resources" => dictionary! {
                "Font" => dictionary! {
                    "F1" => font_id,
                },
            },
        });
        page_ids.push(page_id);
    }

    let kids = page_ids
        .iter()
        .copied()
        .map(Object::Reference)
        .collect::<Vec<_>>();
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(kids),
            "Count" => Object::Integer(page_ids.len() as i64),
        }),
    );

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.compress();
    doc.save(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[derive(Deserialize)]
struct CargoMetadataResponse {
    #[serde(default)]
    packages: Vec<CargoMetadataPackage>,
    #[serde(default)]
    workspace_members: Vec<String>,
    resolve: Option<CargoMetadataResolve>,
}

#[derive(Deserialize)]
struct CargoMetadataPackage {
    id: String,
    name: String,
    version: String,
    repository: Option<String>,
}

#[derive(Deserialize)]
struct CargoMetadataResolve {
    #[serde(default)]
    nodes: Vec<CargoMetadataNode>,
}

#[derive(Deserialize)]
struct CargoMetadataNode {
    id: String,
    #[serde(default)]
    deps: Vec<CargoMetadataNodeDep>,
}

#[derive(Deserialize)]
struct CargoMetadataNodeDep {
    pkg: String,
}

#[derive(Deserialize)]
struct GithubRepoResponse {
    archived: bool,
}

#[derive(Deserialize)]
struct GithubMatchingRef {
    #[serde(rename = "ref")]
    git_ref: String,
}

static CARGO_ARCHIVED_FINDINGS: OnceCell<Vec<String>> = OnceCell::const_new();

async fn cargo_workspace_archived_findings() -> Vec<String> {
    CARGO_ARCHIVED_FINDINGS
        .get_or_init(build_cargo_archived_findings)
        .await
        .clone()
}

async fn build_cargo_archived_findings() -> Vec<String> {
    let output = match tokio::process::Command::new("cargo")
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--locked")
        .output()
        .await
    {
        Ok(output) if output.status.success() => output,
        _ => {
            return vec![
                "Could not evaluate full Cargo dependency tree maintenance status.".to_string(),
            ];
        }
    };

    let metadata: CargoMetadataResponse = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(_) => {
            return vec![
                "Could not parse Cargo metadata for full dependency tree analysis.".to_string(),
            ];
        }
    };

    let package_by_id: HashMap<_, _> = metadata
        .packages
        .into_iter()
        .map(|pkg| (pkg.id.clone(), pkg))
        .collect();

    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(resolve) = metadata.resolve {
        for node in resolve.nodes {
            let deps = node.deps.into_iter().map(|d| d.pkg).collect::<Vec<_>>();
            edges.insert(node.id, deps);
        }
    }

    let roots = if metadata.workspace_members.is_empty() {
        package_by_id.keys().cloned().collect::<Vec<_>>()
    } else {
        metadata.workspace_members
    };

    let mut repo_archived_cache: HashMap<String, bool> = HashMap::new();
    let mut findings = Vec::new();

    for (pkg_id, pkg) in &package_by_id {
        let Some(repo) = pkg
            .repository
            .as_deref()
            .and_then(normalize_repo_url)
            .filter(|url| url.starts_with("https://github.com/"))
        else {
            continue;
        };

        let archived = if let Some(known) = repo_archived_cache.get(&repo) {
            *known
        } else {
            let status = github_repo_is_archived(&repo).await.unwrap_or(false);
            repo_archived_cache.insert(repo.clone(), status);
            status
        };

        if !archived {
            continue;
        }

        if let Some(path) = shortest_dependency_path(&roots, &edges, pkg_id) {
            let path_text = path
                .iter()
                .map(|id| {
                    package_by_id
                        .get(id)
                        .map(|p| p.name.as_str())
                        .unwrap_or("unknown")
                })
                .collect::<Vec<_>>()
                .join(" -> ");
            findings.push(format!(
                "Archived crate detected: {} {} via {}",
                pkg.name, pkg.version, path_text
            ));
        } else {
            findings.push(format!(
                "Archived crate detected: {} {} (path unavailable)",
                pkg.name, pkg.version
            ));
        }
    }

    findings.sort();
    findings.dedup();
    findings
}

async fn github_repo_is_archived(repo_url: &str) -> Option<bool> {
    let path = repo_url
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let client = http::github_client().ok()?;
    let endpoint = format!("https://api.github.com/repos/{owner}/{repo}");
    let response: Result<GithubRepoResponse> =
        http::get_json_with_retries(|| client.get(&endpoint)).await;
    response.ok().map(|r| r.archived)
}

fn shortest_dependency_path(
    roots: &[String],
    edges: &HashMap<String, Vec<String>>,
    target: &str,
) -> Option<Vec<String>> {
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    let mut parent: HashMap<String, String> = HashMap::new();

    for root in roots {
        queue.push_back(root.clone());
        visited.insert(root.clone());
    }

    while let Some(node) = queue.pop_front() {
        if node == target {
            let mut path = vec![node.clone()];
            let mut cur = node;
            while let Some(prev) = parent.get(&cur).cloned() {
                path.push(prev.clone());
                cur = prev;
            }
            path.reverse();
            return Some(path);
        }

        if let Some(children) = edges.get(&node) {
            for child in children {
                if visited.insert(child.clone()) {
                    parent.insert(child.clone(), node.clone());
                    queue.push_back(child.clone());
                }
            }
        }
    }

    None
}

fn wrap_text_lines(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for line in text.lines() {
        if line.len() <= max_chars {
            lines.push(line.to_string());
            continue;
        }

        let mut current = String::new();
        for word in line.split_whitespace() {
            let candidate_len = if current.is_empty() {
                word.len()
            } else {
                current.len() + 1 + word.len()
            };

            if candidate_len > max_chars && !current.is_empty() {
                lines.push(current);
                current = word.to_string();
            } else {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            }
        }

        if !current.is_empty() {
            lines.push(current);
        }
    }
    lines
}

fn classify_semver_change(current: &str, latest: &str) -> String {
    let current_core = extract_version_core(current);
    let latest_core = extract_version_core(latest);

    let Some(current_core) = current_core else {
        return "unknown".to_string();
    };
    let Some(latest_core) = latest_core else {
        return "unknown".to_string();
    };

    let current_ver = semver::Version::parse(current_core.trim_start_matches('v'));
    let latest_ver = semver::Version::parse(latest_core.trim_start_matches('v'));

    let (Ok(current_ver), Ok(latest_ver)) = (current_ver, latest_ver) else {
        return "version-change".to_string();
    };

    if latest_ver.major > current_ver.major {
        "breaking".to_string()
    } else if latest_ver.minor > current_ver.minor {
        "feature".to_string()
    } else if latest_ver.patch > current_ver.patch {
        "bugfix".to_string()
    } else {
        "version-change".to_string()
    }
}

fn extract_version_core(spec: &str) -> Option<String> {
    let start = spec.find(|c: char| c.is_ascii_digit() || c == 'v')?;
    let tail = &spec[start..];
    let len = tail
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | 'v'))
        .count();
    if len == 0 {
        return None;
    }
    Some(tail[..len].to_string())
}

async fn github_compare_link(repo_url: &str, current: &str, latest: &str) -> Option<String> {
    let repo = normalize_repo_url(repo_url)?;
    let (owner, name) = github_repo_owner_name(&repo)?;
    let current_ref = resolve_github_compare_ref(&owner, &name, current).await;
    let latest_ref = resolve_github_compare_ref(&owner, &name, latest).await;

    match (current_ref, latest_ref) {
        (Some(current_ref), Some(latest_ref)) => {
            Some(format!("{repo}/compare/{current_ref}...{latest_ref}"))
        }
        _ => Some(format!(
            "{repo}/compare/{}...{}",
            extract_version_core(current)?,
            extract_version_core(latest)?
        )),
    }
}

fn github_repo_owner_name(repo_url: &str) -> Option<(String, String)> {
    let trimmed = repo_url.strip_prefix("https://github.com/")?;
    let mut parts = trimmed.split('/');
    let owner = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    Some((owner, name))
}

async fn resolve_github_compare_ref(owner: &str, repo: &str, version: &str) -> Option<String> {
    let client = http::github_client().ok()?;
    let candidates = github_ref_candidates(version);

    for candidate in &candidates {
        if github_ref_exists(&client, owner, repo, "tags", candidate).await {
            return Some(candidate.clone());
        }
    }

    for candidate in &candidates {
        if github_ref_exists(&client, owner, repo, "heads", candidate).await {
            return Some(candidate.clone());
        }
    }

    None
}

fn github_ref_candidates(version: &str) -> Vec<String> {
    let mut candidates = Vec::new();

    for value in [
        Some(version.trim().to_string()),
        extract_version_core(version),
    ]
    .into_iter()
    .flatten()
    {
        if value.is_empty() {
            continue;
        }

        candidates.push(value.clone());

        let without_v = value.trim_start_matches('v').to_string();
        if without_v != value {
            candidates.push(without_v.clone());
        }
        if !value.starts_with('v') && value.chars().any(|c| c.is_ascii_digit()) {
            candidates.push(format!("v{value}"));
        }
        if without_v.chars().any(|c| c.is_ascii_digit()) {
            candidates.push(format!("release-{without_v}"));
            candidates.push(format!("release/{without_v}"));
        }
    }

    let mut unique = Vec::new();
    for candidate in candidates {
        if !unique.contains(&candidate) {
            unique.push(candidate);
        }
    }
    unique
}

async fn github_ref_exists(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    kind: &str,
    candidate: &str,
) -> bool {
    let encoded = candidate.replace('/', "%2F");
    let url =
        format!("https://api.github.com/repos/{owner}/{repo}/git/matching-refs/{kind}/{encoded}");
    let response: Result<Vec<GithubMatchingRef>> =
        http::get_json_with_retries(|| client.get(&url)).await;

    match response {
        Ok(refs) => refs
            .into_iter()
            .any(|git_ref| git_ref.git_ref == format!("refs/{kind}/{candidate}")),
        Err(_) => false,
    }
}

fn trim_trailing_slash(value: &str) -> &str {
    value.trim_end_matches('/')
}

fn normalize_repo_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("https://github.com/") {
        return Some(
            trimmed
                .trim_end_matches(".git")
                .trim_end_matches('/')
                .to_string(),
        );
    }
    if let Some(stripped) = trimmed.strip_prefix("git+https://github.com/") {
        return Some(format!(
            "https://github.com/{}",
            stripped.trim_end_matches(".git").trim_end_matches('/')
        ));
    }
    if let Some(stripped) = trimmed.strip_prefix("git://github.com/") {
        return Some(format!(
            "https://github.com/{}",
            stripped.trim_end_matches(".git").trim_end_matches('/')
        ));
    }
    if let Some(stripped) = trimmed.strip_prefix("git@github.com:") {
        return Some(format!(
            "https://github.com/{}",
            stripped.trim_end_matches(".git").trim_end_matches('/')
        ));
    }
    None
}

fn diff_dependency_maps(
    current: &HashMap<String, String>,
    latest: &HashMap<String, String>,
) -> SupplyChainDelta {
    let current_keys: BTreeSet<_> = current.keys().cloned().collect();
    let latest_keys: BTreeSet<_> = latest.keys().cloned().collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();

    for dep in latest_keys.difference(&current_keys) {
        if let Some(req) = latest.get(dep) {
            added.push(format!("{}@{}", dep, req));
        }
    }

    for dep in current_keys.difference(&latest_keys) {
        if let Some(req) = current.get(dep) {
            removed.push(format!("{}@{}", dep, req));
        }
    }

    for dep in latest_keys.intersection(&current_keys) {
        let old = current.get(dep).cloned().unwrap_or_default();
        let new = latest.get(dep).cloned().unwrap_or_default();
        if old != new {
            changed.push(format!("{}: {} -> {}", dep, old, new));
        }
    }

    let note = if added.is_empty() && removed.is_empty() && changed.is_empty() {
        Some("No transitive dependency changes detected.".to_string())
    } else {
        None
    };

    SupplyChainDelta {
        added,
        removed,
        changed,
        note,
    }
}

#[derive(Deserialize)]
struct CargoMetaResponse {
    #[serde(rename = "crate")]
    krate: CargoCrateMeta,
}

#[derive(Deserialize)]
struct CargoCrateMeta {
    repository: Option<String>,
    homepage: Option<String>,
}

#[derive(Deserialize)]
struct CargoDepsResponse {
    dependencies: Vec<CargoVersionDependency>,
}

#[derive(Deserialize)]
struct CargoVersionDependency {
    crate_id: String,
    req: String,
}

async fn cargo_repository(name: &str) -> (Option<String>, Option<String>) {
    let Ok(client) = http::default_client() else {
        return (None, None);
    };

    let url = format!("https://crates.io/api/v1/crates/{name}");
    let response: Result<CargoMetaResponse> =
        http::get_json_with_retries(|| client.get(&url)).await;
    match response {
        Ok(meta) => (meta.krate.repository, meta.krate.homepage),
        Err(_) => (None, None),
    }
}

async fn cargo_version_dependencies(name: &str, version: &str) -> HashMap<String, String> {
    let Ok(client) = http::default_client() else {
        return HashMap::new();
    };

    let url = format!("https://crates.io/api/v1/crates/{name}/{version}/dependencies");
    let response: Result<CargoDepsResponse> =
        http::get_json_with_retries(|| client.get(&url)).await;
    match response {
        Ok(resp) => resp
            .dependencies
            .into_iter()
            .map(|dep| (dep.crate_id, dep.req))
            .collect(),
        Err(_) => HashMap::new(),
    }
}

#[derive(Default)]
struct NpmMeta {
    repository: Option<String>,
    homepage: Option<String>,
}

#[derive(Deserialize)]
struct NpmPackageResponse {
    #[serde(default)]
    repository: serde_json::Value,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    versions: HashMap<String, NpmVersionData>,
}

#[derive(Deserialize)]
struct NpmVersionData {
    #[serde(default)]
    dependencies: HashMap<String, String>,
}

async fn npm_package_meta(name: &str) -> NpmMeta {
    let Ok(client) = http::default_client() else {
        return NpmMeta::default();
    };

    let url = format!("https://registry.npmjs.org/{name}");
    let response: Result<NpmPackageResponse> = http::get_json_with_retries(|| {
        client
            .get(&url)
            .header("Accept", "application/vnd.npm.install-v1+json")
    })
    .await;

    match response {
        Ok(resp) => NpmMeta {
            repository: parse_npm_repo(&resp.repository),
            homepage: resp.homepage,
        },
        Err(_) => NpmMeta::default(),
    }
}

async fn npm_version_dependencies(name: &str, version: &str) -> HashMap<String, String> {
    let Ok(client) = http::default_client() else {
        return HashMap::new();
    };

    let url = format!("https://registry.npmjs.org/{name}");
    let response: Result<NpmPackageResponse> = http::get_json_with_retries(|| {
        client
            .get(&url)
            .header("Accept", "application/vnd.npm.install-v1+json")
    })
    .await;

    match response {
        Ok(resp) => resp
            .versions
            .get(version)
            .map(|v| v.dependencies.clone())
            .unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn parse_npm_repo(value: &serde_json::Value) -> Option<String> {
    if let Some(raw) = value.as_str() {
        return Some(raw.to_string());
    }

    if let Some(obj) = value.as_object()
        && let Some(url) = obj.get("url").and_then(|u| u.as_str())
    {
        return Some(url.to_string());
    }

    None
}

#[derive(Default)]
struct PypiMeta {
    repository: Option<String>,
    changelog: Option<String>,
}

#[derive(Deserialize)]
struct PypiProjectResponse {
    info: PypiProjectInfo,
}

#[derive(Deserialize)]
struct PypiProjectInfo {
    #[serde(default)]
    project_urls: HashMap<String, String>,
    #[serde(default)]
    home_page: Option<String>,
}

#[derive(Deserialize)]
struct PypiVersionResponse {
    info: PypiVersionInfo,
}

#[derive(Deserialize)]
struct PypiVersionInfo {
    #[serde(default)]
    requires_dist: Option<Vec<String>>,
}

async fn pypi_project_meta(name: &str) -> PypiMeta {
    let Ok(client) = http::default_client() else {
        return PypiMeta::default();
    };

    let url = format!("https://pypi.org/pypi/{name}/json");
    let response: Result<PypiProjectResponse> =
        http::get_json_with_retries(|| client.get(&url)).await;

    let Ok(resp) = response else {
        return PypiMeta::default();
    };

    let repo_from_urls = resp
        .info
        .project_urls
        .iter()
        .find(|(key, _)| {
            let lower = key.to_ascii_lowercase();
            lower.contains("source") || lower.contains("repository") || lower.contains("github")
        })
        .map(|(_, value)| value.clone());

    let changelog = resp
        .info
        .project_urls
        .iter()
        .find(|(key, _)| {
            let lower = key.to_ascii_lowercase();
            lower.contains("changelog") || lower.contains("release") || lower.contains("news")
        })
        .map(|(_, value)| value.clone());

    let repository = repo_from_urls.or(resp.info.home_page);

    PypiMeta {
        repository,
        changelog,
    }
}

async fn pypi_version_dependencies(name: &str, version: &str) -> HashMap<String, String> {
    let Ok(client) = http::default_client() else {
        return HashMap::new();
    };

    let url = format!("https://pypi.org/pypi/{name}/{version}/json");
    let response: Result<PypiVersionResponse> =
        http::get_json_with_retries(|| client.get(&url)).await;

    match response {
        Ok(resp) => parse_requires_dist(resp.info.requires_dist.unwrap_or_default()),
        Err(_) => HashMap::new(),
    }
}

fn parse_requires_dist(items: Vec<String>) -> HashMap<String, String> {
    let mut out = HashMap::new();

    for raw in items {
        let trimmed = raw.split(';').next().unwrap_or(raw.as_str()).trim();
        if trimmed.is_empty() {
            continue;
        }

        let version_start = trimmed.find(['>', '<', '=', '!', '~']);
        match version_start {
            Some(pos) => {
                let name = trimmed[..pos]
                    .split('[')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let req = trimmed[pos..].trim().to_string();
                if !name.is_empty() {
                    out.insert(name, req);
                }
            }
            None => {
                let name = trimmed.split('[').next().unwrap_or("").trim().to_string();
                if !name.is_empty() {
                    out.insert(name, "*".to_string());
                }
            }
        }
    }

    out
}
