mod cli;
mod config;
mod http;
mod plugin;
mod plugins;
mod progress;
mod report;

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use dialoguer::console::{Key, Term};

use cli::{Cli, Commands, ReportFormatArg};
use plugin::{Dependency, Plugin};
use plugins::cargo::CargoPlugin;
use plugins::docker::DockerPlugin;
use plugins::github_actions::GithubActionsPlugin;
use plugins::npm::NpmPlugin;
use plugins::pyproject::PyprojectPlugin;
use plugins::requirements::RequirementsPlugin;

fn all_plugins() -> Vec<Box<dyn Plugin>> {
    vec![
        Box::new(CargoPlugin::new()),
        Box::new(DockerPlugin::new()),
        Box::new(GithubActionsPlugin::new()),
        Box::new(NpmPlugin::new()),
        Box::new(PyprojectPlugin::new()),
        Box::new(RequirementsPlugin::new()),
    ]
}

fn detect_plugins(dir: &Path, only: &Option<Vec<String>>) -> Vec<Box<dyn Plugin>> {
    all_plugins()
        .into_iter()
        .filter(|p| p.detect(dir))
        .filter(|p| {
            only.as_ref().is_none_or(|names| {
                names.iter().any(|n| {
                    let n = n.to_ascii_lowercase();
                    n == p.name().to_ascii_lowercase()
                        || n == "actions" && p.name() == "github-actions"
                        || n == "pnpm" && p.name() == "npm"
                        || n == "yarn" && p.name() == "npm"
                })
            })
        })
        .collect()
}

/// Walk the directory tree rooted at `root`, respecting `.gitignore`, and return
/// every (plugin, directory) pair where a plugin detects a manifest.
fn walk_all_plugin_dirs(
    root: &Path,
    only: &Option<Vec<String>>,
) -> Vec<(Box<dyn Plugin>, PathBuf)> {
    let mut results = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .hidden(false) // include dotfiles/dot-dirs like .github/
        .build()
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().map_or(false, |ft| ft.is_dir()) {
            continue;
        }
        for plugin in detect_plugins(entry.path(), only) {
            results.push((plugin, entry.path().to_path_buf()));
        }
    }
    results
}

/// Build the section header label for a plugin, appending the relative path when
/// the manifest lives in a subdirectory.
fn plugin_label(plugin: &dyn Plugin, dir: &Path, root: &Path) -> String {
    let base = plugin.display_name(dir);
    match dir.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => format!("{} [{}]", base, rel.display()),
        _ => base,
    }
}

use plugin::DepType;

fn map_report_format(value: &ReportFormatArg) -> report::ReportFormat {
    match value {
        ReportFormatArg::Text => report::ReportFormat::Text,
        ReportFormatArg::Markdown => report::ReportFormat::Markdown,
        ReportFormatArg::Html => report::ReportFormat::Html,
        ReportFormatArg::Pdf => report::ReportFormat::Pdf,
    }
}

async fn open_interactive_report_for_plugins(
    plugin_deps: &[(String, Vec<Dependency>)],
) -> Result<String> {
    let mut plugin_reports = Vec::new();
    let mut total_candidates = 0usize;
    let plugin_count = plugin_deps
        .iter()
        .filter(|(_, deps)| !deps.is_empty())
        .count() as u64;

    if plugin_count == 0 {
        return Ok(format!(
            "{} no upgrade candidates available for report",
            "⚠".yellow()
        ));
    }

    let pb = progress::report_progress_bar(plugin_count);

    for (plugin_name, deps) in plugin_deps {
        if deps.is_empty() {
            continue;
        }
        pb.set_message(plugin_name.to_string());
        total_candidates += deps.len();
        let entries = report::build_dependency_reports(plugin_name, deps.clone()).await;
        let security_findings = report::plugin_security_findings(plugin_name).await;
        plugin_reports.push(report::PluginReport {
            plugin: plugin_name.clone(),
            security_findings,
            dependencies: entries,
        });
        pb.inc(1);
    }
    pb.finish_and_clear();

    let generated_at_utc = format!("{:?}", std::time::SystemTime::now());
    let bundle = report::UpgradeReport {
        generated_at_utc,
        plugins: plugin_reports,
    };

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let output_base = format!("ruckup-update-report-{timestamp}");
    let paths = report::write_reports(&bundle, &[report::ReportFormat::Html], &output_base)?;
    let opened = report::open_best_effort(&paths);

    let message = if let Some(path) = paths.first() {
        if opened {
            format!(
                "{} opened upgrade report ({} candidates): {}",
                "✓".green(),
                total_candidates,
                path.display()
            )
        } else {
            format!(
                "{} generated upgrade report ({} candidates): {} (auto-open failed)",
                "⚠".yellow(),
                total_candidates,
                path.display()
            )
        }
    } else {
        format!(
            "{} generated upgrade report ({} candidates)",
            "✓".green(),
            total_candidates,
        )
    };

    Ok(message)
}

async fn select_dependencies_interactive(
    plugin_label: &str,
    updatable: &[Dependency],
    report_candidates: &[(String, Vec<Dependency>)],
) -> Result<Vec<String>> {
    let term = Term::stderr();
    let mut cursor = 0usize;
    let mut selected = vec![false; updatable.len()];
    let mut rendered_lines = 0usize;
    let mut status_message: Option<String> = None;

    loop {
        if rendered_lines > 0 {
            term.clear_last_lines(rendered_lines)?;
        }

        let mut lines = 0usize;
        term.write_line(&format!(
            "Select dependencies to update for {}",
            plugin_label
        ))?;
        lines += 1;
        term.write_line(&format!(
            "  {}: r open report, ↑/↓ navigate, Space toggle, a toggle all, Enter confirm, Esc skip",
            "keys".dimmed(),
        ))?;
        lines += 1;

        if let Some(msg) = &status_message {
            term.write_line(&format!("  {}", msg))?;
            lines += 1;
        }

        for (idx, dep) in updatable.iter().enumerate() {
            let latest = dep.latest_version.as_deref().unwrap_or("?");
            let marker = if idx == cursor { ">" } else { " " };
            let check = if selected[idx] { "x" } else { " " };
            term.write_line(&format!(
                "  {} [{}] {} {} → {}",
                marker, check, dep.name, dep.current_version, latest
            ))?;
            lines += 1;
        }

        rendered_lines = lines;
        status_message = None;
        term.flush()?;

        match term.read_key_raw()? {
            Key::ArrowUp | Key::Char('k') => {
                if cursor == 0 {
                    cursor = updatable.len().saturating_sub(1);
                } else {
                    cursor -= 1;
                }
            }
            Key::ArrowDown | Key::Char('j') if !updatable.is_empty() => {
                cursor = (cursor + 1) % updatable.len();
            }
            Key::Char(' ') if !updatable.is_empty() => {
                selected[cursor] = !selected[cursor];
            }
            Key::Char('a') | Key::Char('A') => {
                let select_all = selected.iter().any(|v| !*v);
                selected.fill(select_all);
            }
            Key::Char('r') | Key::Char('R') => {
                if rendered_lines > 0 {
                    term.clear_last_lines(rendered_lines)?;
                    rendered_lines = 0;
                }
                term.write_line(&format!("  {} generating upgrade report...", "•".dimmed()))?;
                term.flush()?;
                let started = std::time::Instant::now();
                let status = open_interactive_report_for_plugins(report_candidates).await?;
                let elapsed = started.elapsed().as_millis();
                status_message = Some(format!("{} ({} ms)", status, elapsed));
            }
            Key::Enter => {
                let chosen = selected
                    .iter()
                    .enumerate()
                    .filter_map(|(i, checked)| {
                        if *checked {
                            Some(updatable[i].name.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();

                if chosen.is_empty() {
                    status_message = Some("No dependencies selected.".to_string());
                    continue;
                }

                if rendered_lines > 0 {
                    term.clear_last_lines(rendered_lines)?;
                }
                return Ok(chosen);
            }
            Key::Escape | Key::Char('q') => {
                if rendered_lines > 0 {
                    term.clear_last_lines(rendered_lines)?;
                }
                return Ok(Vec::new());
            }
            _ => {}
        }
    }
}

fn print_dep_table(deps: &[Dependency]) {
    if deps.is_empty() {
        println!("  No dependencies found.");
        return;
    }

    let updatable: Vec<_> = deps.iter().filter(|d| d.has_update()).collect();
    let up_to_date = deps.len() - updatable.len();

    if updatable.is_empty() {
        println!(
            "  {} {} dependencies are up to date.",
            "✓".green(),
            deps.len()
        );
        return;
    }

    println!(
        "  {} updates available ({} up to date):\n",
        updatable.len().to_string().yellow(),
        up_to_date
    );

    // Group by dep type for clearer display
    let groups: &[DepType] = &[
        DepType::Normal,
        DepType::Dev,
        DepType::Build,
        DepType::Optional,
    ];

    // Calculate column widths across all groups
    let max_name = updatable.iter().map(|d| d.name.len()).max().unwrap_or(10);
    let max_current = updatable
        .iter()
        .map(|d| d.current_version.len())
        .max()
        .unwrap_or(10);

    // Compute which deps are blocking others (for annotation)
    use std::collections::HashMap;
    let mut holding_back: HashMap<&str, Vec<&str>> = HashMap::new();
    for dep in deps {
        for conflict in &dep.held_back_by {
            holding_back
                .entry(conflict.blocker.as_str())
                .or_default()
                .push(dep.name.as_str());
        }
    }

    for group in groups {
        let group_deps: Vec<_> = updatable.iter().filter(|d| &d.dep_type == group).collect();
        if group_deps.is_empty() {
            continue;
        }
        println!("  {}:", group.label().dimmed());
        for dep in &group_deps {
            let latest = dep.latest_version.as_deref().unwrap_or("?");
            let annotation = if !dep.held_back_by.is_empty() {
                let blockers: Vec<_> = dep
                    .held_back_by
                    .iter()
                    .map(|c| c.blocker.as_str())
                    .collect();
                format!(
                    "  {}",
                    format!("⚠ held back by {}", blockers.join(", ")).yellow()
                )
            } else if let Some(blocked) = holding_back.get(dep.name.as_str()) {
                format!("  {}", format!("⚑ blocks {}", blocked.join(", ")).dimmed())
            } else {
                String::new()
            };
            println!(
                "    {:<width_name$}  {:<width_cur$}  →  {}{}",
                dep.name.bold(),
                dep.current_version.red(),
                latest.green(),
                annotation,
                width_name = max_name,
                width_cur = max_current,
            );
        }
    }

    // Peer dependency conflict summary
    let peer_conflicts: Vec<_> = deps.iter().filter(|d| !d.held_back_by.is_empty()).collect();
    if !peer_conflicts.is_empty() {
        println!(
            "  {} {}:",
            "⚠".yellow(),
            "peer dependency conflicts".yellow().bold()
        );
        for dep in &peer_conflicts {
            for conflict in &dep.held_back_by {
                let latest = dep.latest_version.as_deref().unwrap_or("?");
                println!(
                    "    {}@{} requires {}@{}",
                    conflict.blocker.bold(),
                    conflict.blocker_version,
                    dep.name,
                    conflict.required_range,
                );
                println!(
                    "      {} conflicts with {}@{}",
                    "→".dimmed(),
                    dep.name,
                    latest,
                );
            }
        }
    }

    println!();
}

async fn cmd_check(
    dir: &Path,
    only: &Option<Vec<String>>,
    filter: &Option<Vec<String>>,
    config: &config::Config,
) -> Result<()> {
    let plugin_dirs = walk_all_plugin_dirs(dir, only);
    if plugin_dirs.is_empty() {
        println!(
            "{}",
            "No supported dependency files detected in the current directory.".yellow()
        );
        return Ok(());
    }

    for (plugin, pdir) in &plugin_dirs {
        println!("{}", format!("── {} ──", plugin_label(plugin.as_ref(), pdir, dir)).bold());
        let mut deps = plugin.check_updates(pdir, config).await?;
        let total_checked = deps.len();
        let failed_checks = deps.iter().filter(|d| d.check_failed).count();
        if let Some(names) = filter {
            deps.retain(|d| names.iter().any(|n| d.name.contains(n.as_str())));
        }
        print_dep_table(&deps);
        if failed_checks > 0 {
            println!(
                "  {} Checked {}/{} dependencies successfully ({} failed lookups).\n",
                "⚠".yellow(),
                total_checked - failed_checks,
                total_checked,
                failed_checks,
            );
        }
    }
    Ok(())
}

async fn cmd_list(dir: &Path, only: &Option<Vec<String>>) -> Result<()> {
    let plugin_dirs = walk_all_plugin_dirs(dir, only);
    if plugin_dirs.is_empty() {
        println!(
            "{}",
            "No supported dependency files detected in the current directory.".yellow()
        );
        return Ok(());
    }

    for (plugin, pdir) in &plugin_dirs {
        println!("{}", format!("── {} ──", plugin_label(plugin.as_ref(), pdir, dir)).bold());
        let deps = plugin.list_dependencies(pdir).await?;
        if deps.is_empty() {
            println!("  No dependencies found.\n");
            continue;
        }
        let max_name = deps.iter().map(|d| d.name.len()).max().unwrap_or(10);
        let groups: &[DepType] = &[
            DepType::Normal,
            DepType::Dev,
            DepType::Build,
            DepType::Optional,
        ];
        for group in groups {
            let group_deps: Vec<_> = deps.iter().filter(|d| d.dep_type == *group).collect();
            if group_deps.is_empty() {
                continue;
            }
            println!("  {}:", group.label().dimmed());
            for dep in &group_deps {
                println!(
                    "    {:<width$}  {}",
                    dep.name,
                    dep.current_version,
                    width = max_name,
                );
            }
        }
        println!();
    }
    Ok(())
}

async fn cmd_update(
    dir: &Path,
    only: &Option<Vec<String>>,
    filter: &Option<Vec<String>>,
    update_all: bool,
    config: &config::Config,
) -> Result<()> {
    let plugin_dirs = walk_all_plugin_dirs(dir, only);
    if plugin_dirs.is_empty() {
        println!(
            "{}",
            "No supported dependency files detected in the current directory.".yellow()
        );
        return Ok(());
    }

    let mut deps_by_entry: Vec<Vec<Dependency>> = Vec::new();
    let mut totals_by_entry: Vec<usize> = Vec::new();
    let mut failed_by_entry: Vec<usize> = Vec::new();

    for (plugin, pdir) in &plugin_dirs {
        let mut deps = plugin.check_updates(pdir, config).await?;
        let total_checked = deps.len();
        let failed_checks = deps.iter().filter(|d| d.check_failed).count();
        if let Some(names) = filter {
            deps.retain(|d| names.iter().any(|n| d.name.contains(n.as_str())));
        }
        deps_by_entry.push(deps);
        totals_by_entry.push(total_checked);
        failed_by_entry.push(failed_checks);
    }

    let report_candidates: Vec<(String, Vec<Dependency>)> = plugin_dirs
        .iter()
        .zip(deps_by_entry.iter())
        .filter_map(|((plugin, pdir), deps)| {
            let updatable: Vec<Dependency> =
                deps.iter().filter(|d| d.has_update()).cloned().collect();
            if updatable.is_empty() {
                None
            } else {
                Some((plugin_label(plugin.as_ref(), pdir, dir), updatable))
            }
        })
        .collect();

    for (idx, (plugin, pdir)) in plugin_dirs.iter().enumerate() {
        println!("{}", format!("── {} ──", plugin_label(plugin.as_ref(), pdir, dir)).bold());

        let deps = &deps_by_entry[idx];
        let total_checked = totals_by_entry[idx];
        let failed_checks = failed_by_entry[idx];
        let updatable: Vec<_> = deps.iter().filter(|d| d.has_update()).collect();

        if failed_checks > 0 {
            println!(
                "  {} Checked {}/{} dependencies successfully ({} failed lookups).",
                "⚠".yellow(),
                total_checked - failed_checks,
                total_checked,
                failed_checks,
            );
        }

        print_dep_table(deps);

        if updatable.is_empty() {
            continue;
        }
    }

    if report_candidates.is_empty() {
        return Ok(());
    }

    if !update_all {
        println!(
            "  {}: r open report, ↑/↓ navigate, Space toggle, a toggle all, Enter confirm\n",
            "keys".dimmed(),
        );
    }

    for (idx, (plugin, pdir)) in plugin_dirs.iter().enumerate() {
        let deps = &deps_by_entry[idx];
        let updatable: Vec<Dependency> = deps.iter().filter(|d| d.has_update()).cloned().collect();
        if updatable.is_empty() {
            continue;
        }

        let label = plugin_label(plugin.as_ref(), pdir, dir);
        let selected_names: Vec<String> = if update_all {
            updatable.iter().map(|d| d.name.clone()).collect()
        } else {
            let selected = select_dependencies_interactive(
                label.as_ref(),
                &updatable,
                &report_candidates,
            )
            .await?;

            if selected.is_empty() {
                println!("  No dependencies selected.\n");
                continue;
            }

            selected
        };

        plugin.update(pdir, &selected_names, config).await?;
        println!(
            "  {} Updated {} dependencies.\n",
            "✓".green(),
            selected_names.len()
        );
    }
    Ok(())
}

async fn cmd_report(
    dir: &Path,
    only: &Option<Vec<String>>,
    filter: &Option<Vec<String>>,
    formats: &[ReportFormatArg],
    output: &str,
    open: bool,
    config: &config::Config,
) -> Result<()> {
    let plugin_dirs = walk_all_plugin_dirs(dir, only);
    if plugin_dirs.is_empty() {
        println!(
            "{}",
            "No supported dependency files detected in the current directory.".yellow()
        );
        return Ok(());
    }

    let mut plugin_reports = Vec::new();
    let mut total_candidates = 0usize;

    for (plugin, pdir) in &plugin_dirs {
        println!("{}", format!("── {} ──", plugin_label(plugin.as_ref(), pdir, dir)).bold());

        let mut deps = plugin.check_updates(pdir, config).await?;
        if let Some(names) = filter {
            deps.retain(|d| names.iter().any(|n| d.name.contains(n.as_str())));
        }

        let updatable: Vec<_> = deps.into_iter().filter(|d| d.has_update()).collect();
        if updatable.is_empty() {
            println!("  {} No upgrade candidates for report.\n", "✓".green());
            continue;
        }

        total_candidates += updatable.len();
        println!(
            "  {} Building detailed report for {} dependencies...",
            "•".dimmed(),
            updatable.len()
        );

        let label = plugin_label(plugin.as_ref(), pdir, dir);
        let entries = report::build_dependency_reports(&label, updatable).await;
        let security_findings = report::plugin_security_findings(plugin.name()).await;
        plugin_reports.push(report::PluginReport {
            plugin: label,
            security_findings,
            dependencies: entries,
        });
        println!("  {} Done.\n", "✓".green());
    }

    if plugin_reports.is_empty() {
        println!(
            "{}",
            "No upgradable dependencies found, so no report was generated.".yellow()
        );
        return Ok(());
    }

    let generated_at_utc = format!("{:?}", std::time::SystemTime::now());
    let report = report::UpgradeReport {
        generated_at_utc,
        plugins: plugin_reports,
    };

    let selected_formats: Vec<_> = formats.iter().map(map_report_format).collect();
    let output_paths = report::write_reports(&report, &selected_formats, output)?;

    println!(
        "{} Generated report for {} upgrade candidates:",
        "✓".green(),
        total_candidates
    );
    for path in &output_paths {
        println!("  - {}", path.display());
    }

    if open && !report::open_best_effort(&output_paths) {
        println!(
            "{} Could not auto-open report. Open one of the paths above manually.",
            "⚠".yellow()
        );
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let dir = PathBuf::from(".");
    let cfg = config::load(&dir);

    match cli.command {
        None | Some(Commands::Check) => cmd_check(&dir, &cli.only, &cli.filter, &cfg).await,
        Some(Commands::List) => cmd_list(&dir, &cli.only).await,
        Some(Commands::Update { all }) => cmd_update(&dir, &cli.only, &cli.filter, all, &cfg).await,
        Some(Commands::Report {
            format,
            output,
            open,
        }) => cmd_report(&dir, &cli.only, &cli.filter, &format, &output, open, &cfg).await,
    }
}
