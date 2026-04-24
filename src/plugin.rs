use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepType {
    Normal,
    Dev,
    Build,
    Optional,
}

impl DepType {
    pub fn label(&self) -> &'static str {
        match self {
            DepType::Normal => "dependencies",
            DepType::Dev => "dev-dependencies",
            DepType::Build => "build-dependencies",
            DepType::Optional => "optional",
        }
    }

    pub fn short_label(&self) -> &'static str {
        match self {
            DepType::Normal => "prod",
            DepType::Dev => "dev",
            DepType::Build => "build",
            DepType::Optional => "opt",
        }
    }
}

impl std::fmt::Display for DepType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.short_label())
    }
}

#[derive(Debug, Clone)]
pub struct PeerConflict {
    /// Package that imposes the peer dependency constraint.
    pub blocker: String,
    /// Version of the blocker that has the constraint.
    pub blocker_version: String,
    /// The peer dependency version range required.
    pub required_range: String,
}

#[derive(Debug, Clone)]
pub struct Dependency {
    pub name: String,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub dep_type: DepType,
    /// Whether the latest version satisfies the current version spec.
    pub satisfied: bool,
    /// Peer dependencies of this package's latest version (name, range).
    pub peer_deps: Vec<(String, String)>,
    /// Peer dependency conflicts: other packages that constrain this one.
    pub held_back_by: Vec<PeerConflict>,
    /// True when registry/API lookup failed for this dependency.
    pub check_failed: bool,
}

impl Dependency {
    pub fn new(name: String, current_version: String, dep_type: DepType) -> Self {
        Self {
            name,
            current_version,
            latest_version: None,
            dep_type,
            satisfied: false,
            peer_deps: Vec::new(),
            held_back_by: Vec::new(),
            check_failed: false,
        }
    }

    pub fn has_update(&self) -> bool {
        self.latest_version.is_some() && !self.satisfied
    }
}

#[async_trait]
pub trait Plugin: Send + Sync {
    /// Human-readable name for this plugin (e.g. "Cargo", "npm").
    fn name(&self) -> &str;

    /// The manifest file name this plugin looks for (e.g. "Cargo.toml").
    fn file_name(&self) -> &str;

    /// Returns true if the manifest file exists in `dir`.
    fn detect(&self, dir: &Path) -> bool {
        let path = dir.join(self.file_name());
        path.is_file() || path.is_dir()
    }

    /// Display name for headers, can include extra context (e.g. detected lock file).
    fn display_name(&self, _dir: &Path) -> String {
        format!("{} ({})", self.name(), self.file_name())
    }

    /// Parse the manifest and list all dependencies with their current versions.
    async fn list_dependencies(&self, dir: &Path) -> Result<Vec<Dependency>>;

    /// Check registries for latest versions of all dependencies.
    async fn check_updates(&self, dir: &Path, config: &Config) -> Result<Vec<Dependency>>;

    /// Update the specified dependencies in-place in the manifest file.
    /// If `deps` is empty, update all that have newer versions.
    async fn update(&self, dir: &Path, deps: &[String], config: &Config) -> Result<()>;
}
