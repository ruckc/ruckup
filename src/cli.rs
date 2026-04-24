use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ruckup",
    version,
    about = "Check and update dependencies across package managers"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Only check these specific package managers (cargo, github-actions, npm, pyproject, requirements)
    #[arg(short, long, value_delimiter = ',', global = true)]
    pub only: Option<Vec<String>>,

    /// Filter to specific dependency names
    #[arg(short, long, value_delimiter = ',', global = true)]
    pub filter: Option<Vec<String>>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Check for available dependency updates (default)
    Check,
    /// Interactively select and apply dependency updates
    Update {
        /// Update all without prompting
        #[arg(short, long)]
        all: bool,
    },
    /// List detected dependency files and their dependencies
    List,
}
