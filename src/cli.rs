use clap::{Parser, Subcommand, ValueEnum};

#[derive(Clone, Debug, ValueEnum)]
pub enum ReportFormatArg {
    Text,
    Markdown,
    Html,
    Pdf,
}

#[derive(Parser)]
#[command(
    name = "ruckup",
    version,
    about = "Check and update dependencies across package managers"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Only check these specific package managers (cargo, docker, github-actions, npm, pyproject, requirements)
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
    /// Generate upgrade intelligence reports
    Report {
        /// Output formats (text, markdown, html, pdf)
        #[arg(
            short = 'F',
            long = "format",
            value_enum,
            value_delimiter = ',',
            default_value = "markdown"
        )]
        format: Vec<ReportFormatArg>,

        /// Output path: file stem (default) or directory
        #[arg(short = 'O', long, default_value = "ruckup-report")]
        output: String,

        /// Open the generated report in your default browser/app
        #[arg(long)]
        open: bool,
    },
}
