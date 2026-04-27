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
use dialoguer::MultiSelect;
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
) -> Result<()> {
    let mut plugin_reports = Vec::new();
    let mut total_candidates = 0usize;

    for (plugin_name, deps) in plugin_deps {
        if deps.is_empty() {
            continue;
        }
        total_candidates += deps.len();
        let entries = report::build_dependency_reports(plugin_name, deps.clone()).await;
        plugin_reports.push(report::PluginReport {
            plugin: plugin_name.clone(),
            dependencies: entries,
        });
    }

    if plugin_reports.is_empty() {
        println!(
            "  {} no upgrade candidates available for report",
            "⚠".yellow()
        );
        return Ok(());
    }

    let generated_at_utc = format!("{:?}", std::time::SystemTime::now());
    let bundle = report::UpgradeReport {
        generated_at_utc,
        plugins: plugin_reports,
    };

    let output_base = "ruckup-update-report-consolidated";
    let paths = report::write_reports(&bundle, &[report::ReportFormat::Html], &output_base)?;
    report::open_best_effort(&paths);

    if let Some(path) = paths.first() {
        println!(
            "  {} opened consolidated report ({} candidates): {}",
            "✓".green(),
            total_candidates,
            path.display()
        );
    }

    Ok(())
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
    let plugins = detect_plugins(dir, only);
    if plugins.is_empty() {
        println!(
            "{}",
            "No supported dependency files detected in the current directory.".yellow()
        );
        return Ok(());
    }

    for plugin in &plugins {
        println!("{}", format!("── {} ──", plugin.display_name(dir)).bold());
        let mut deps = plugin.check_updates(dir, config).await?;
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
    let plugins = detect_plugins(dir, only);
    if plugins.is_empty() {
        println!(
            "{}",
            "No supported dependency files detected in the current directory.".yellow()
        );
        return Ok(());
    }

    for plugin in &plugins {
        println!("{}", format!("── {} ──", plugin.display_name(dir)).bold());
        let deps = plugin.list_dependencies(dir).await?;
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
    let plugins = detect_plugins(dir, only);
    if plugins.is_empty() {
        println!(
            "{}",
            "No supported dependency files detected in the current directory.".yellow()
        );
        return Ok(());
    }

    let mut deps_by_plugin: Vec<Vec<Dependency>> = Vec::new();
    let mut totals_by_plugin: Vec<usize> = Vec::new();
    let mut failed_by_plugin: Vec<usize> = Vec::new();

    for plugin in &plugins {
        let mut deps = plugin.check_updates(dir, config).await?;
        let total_checked = deps.len();
        let failed_checks = deps.iter().filter(|d| d.check_failed).count();
        if let Some(names) = filter {
            deps.retain(|d| names.iter().any(|n| d.name.contains(n.as_str())));
        }
        deps_by_plugin.push(deps);
        totals_by_plugin.push(total_checked);
        failed_by_plugin.push(failed_checks);
    }

    let consolidated_candidates: Vec<(String, Vec<Dependency>)> = plugins
        .iter()
        .zip(deps_by_plugin.iter())
        .filter_map(|(plugin, deps)| {
            let updatable: Vec<Dependency> =
                deps.iter().filter(|d| d.has_update()).cloned().collect();
            if updatable.is_empty() {
                None
            } else {
                Some((plugin.name().to_string(), updatable))
            }
        })
        .collect();

    for (idx, plugin) in plugins.iter().enumerate() {
        println!("{}", format!("── {} ──", plugin.display_name(dir)).bold());

        let deps = &deps_by_plugin[idx];
        let total_checked = totals_by_plugin[idx];
        let failed_checks = failed_by_plugin[idx];

        if failed_checks > 0 {
            println!(
                "  {} Checked {}/{} dependencies successfully ({} failed lookups).",
                "⚠".yellow(),
                total_checked - failed_checks,
                total_checked,
                failed_checks,
            );
        }

        let updatable: Vec<_> = deps.iter().filter(|d| d.has_update()).collect();
        if updatable.is_empty() {
            println!("  {} All dependencies are up to date.\n", "✓".green());
            continue;
        }

        let selected_names: Vec<String> = if update_all {
            updatable.iter().map(|d| d.name.clone()).collect()
        } else {
            let items: Vec<String> = updatable
                .iter()
                .map(|d| {
                    let latest = d.latest_version.as_deref().unwrap_or("?");
                    format!("{} {} → {}", d.name, d.current_version, latest)
                })
                .collect();

            println!(
                "  {}: r open report, ↑/↓ navigate, Space toggle, a toggle all, Enter confirm\n",
                "keys".dimmed(),
            );

            if let Ok(key) = Term::stderr().read_key()
                && matches!(key, Key::Char('r') | Key::Char('R'))
            {
                open_interactive_report_for_plugins(&consolidated_candidates).await?;
                println!();
            }

            let selections = MultiSelect::new()
                .with_prompt("Select dependencies to update")
                .items(&items)
                .interact()?;

            if selections.is_empty() {
                println!("  No dependencies selected.\n");
                continue;
            }

            selections
                .iter()
                .map(|&i| updatable[i].name.clone())
                .collect()
        };

        plugin.update(dir, &selected_names, config).await?;
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
    let plugins = detect_plugins(dir, only);
    if plugins.is_empty() {
        println!(
            "{}",
            "No supported dependency files detected in the current directory.".yellow()
        );
        return Ok(());
    }

    let mut plugin_reports = Vec::new();
    let mut total_candidates = 0usize;

    for plugin in &plugins {
        println!("{}", format!("── {} ──", plugin.display_name(dir)).bold());

        let mut deps = plugin.check_updates(dir, config).await?;
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

        let entries = report::build_dependency_reports(plugin.name(), updatable).await;
        plugin_reports.push(report::PluginReport {
            plugin: plugin.name().to_string(),
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

    if open {
        report::open_best_effort(&output_paths);
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
