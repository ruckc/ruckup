mod cli;
mod config;
mod http;
mod plugin;
mod plugins;
mod progress;

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use dialoguer::MultiSelect;

use cli::{Cli, Commands};
use plugin::{Dependency, Plugin};
use plugins::cargo::CargoPlugin;
use plugins::github_actions::GithubActionsPlugin;
use plugins::npm::NpmPlugin;
use plugins::pyproject::PyprojectPlugin;

fn all_plugins() -> Vec<Box<dyn Plugin>> {
    vec![
        Box::new(CargoPlugin::new()),
        Box::new(GithubActionsPlugin::new()),
        Box::new(NpmPlugin::new()),
        Box::new(PyprojectPlugin::new()),
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

    for plugin in &plugins {
        println!("{}", format!("── {} ──", plugin.display_name(dir)).bold());

        let mut deps = plugin.check_updates(dir, config).await?;
        let total_checked = deps.len();
        let failed_checks = deps.iter().filter(|d| d.check_failed).count();
        if let Some(names) = filter {
            deps.retain(|d| names.iter().any(|n| d.name.contains(n.as_str())));
        }

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
                "  {}: ↑/↓ navigate, Space toggle, a toggle all, Enter confirm\n",
                "keys".dimmed(),
            );

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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let dir = PathBuf::from(".");
    let cfg = config::load(&dir);

    match cli.command {
        None | Some(Commands::Check) => cmd_check(&dir, &cli.only, &cli.filter, &cfg).await,
        Some(Commands::List) => cmd_list(&dir, &cli.only).await,
        Some(Commands::Update { all }) => cmd_update(&dir, &cli.only, &cli.filter, all, &cfg).await,
    }
}
