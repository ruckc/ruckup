use std::path::{Path, PathBuf};

use serde::Deserialize;

/// All configuration keys supported by ruckup.
///
/// Resolution order (later wins):
/// 1. Built-in defaults
/// 2. `~/.ruckuprc` (global)
/// 3. `./.ruckuprc` (project-local)
/// 4. Environment variables (`RUCKUP_*`)
#[derive(Debug, Clone)]
pub struct Config {
    /// Whether to preserve range prefixes (^, ~, >=, etc.) when updating versions.
    /// Default: true
    pub preserve_range: bool,

    /// Max concurrent registry lookups for Cargo/crates.io.
    /// crates.io has strict rate limits, so keep this conservative.
    /// Default: 5
    pub cargo_concurrency: usize,

    /// Max concurrent registry lookups for npm/pnpm/yarn.
    /// npm registry is more lenient.
    /// Default: 16
    pub npm_concurrency: usize,

    /// Max concurrent registry lookups for PyPI.
    /// Default: 10
    pub pypi_concurrency: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            preserve_range: true,
            cargo_concurrency: 5,
            npm_concurrency: 16,
            pypi_concurrency: 10,
        }
    }
}

/// Raw representation of the rc file, with all fields optional so partial
/// files merge cleanly.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RcFile {
    preserve_range: Option<bool>,
    cargo_concurrency: Option<usize>,
    npm_concurrency: Option<usize>,
    pypi_concurrency: Option<usize>,
}

impl RcFile {
    fn load(path: &Path) -> Option<Self> {
        let content = std::fs::read_to_string(path).ok()?;
        // Support both JSON and TOML formats
        if let Ok(rc) = serde_json::from_str::<RcFile>(&content) {
            return Some(rc);
        }
        if let Ok(rc) = toml::from_str::<RcFile>(&content) {
            return Some(rc);
        }
        eprintln!(
            "  warning: failed to parse config file {}",
            path.display()
        );
        None
    }

    /// Apply non-None fields from this file onto `config`.
    fn apply_to(&self, config: &mut Config) {
        if let Some(v) = self.preserve_range {
            config.preserve_range = v;
        }
        if let Some(v) = self.cargo_concurrency {
            config.cargo_concurrency = v.max(1);
        }
        if let Some(v) = self.npm_concurrency {
            config.npm_concurrency = v.max(1);
        }
        if let Some(v) = self.pypi_concurrency {
            config.pypi_concurrency = v.max(1);
        }
    }
}

/// Environment variable layer. Each supported key maps to `RUCKUP_<UPPER_SNAKE>`.
struct EnvLayer;

impl EnvLayer {
    fn apply_to(config: &mut Config) {
        if let Some(v) = Self::read_bool("RUCKUP_PRESERVE_RANGE") {
            config.preserve_range = v;
        }
        if let Some(v) = Self::read_usize("RUCKUP_CARGO_CONCURRENCY") {
            config.cargo_concurrency = v;
        }
        if let Some(v) = Self::read_usize("RUCKUP_NPM_CONCURRENCY") {
            config.npm_concurrency = v;
        }
        if let Some(v) = Self::read_usize("RUCKUP_PYPI_CONCURRENCY") {
            config.pypi_concurrency = v;
        }
    }

    fn read_bool(key: &str) -> Option<bool> {
        let val = std::env::var(key).ok()?;
        match val.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => {
                eprintln!("  warning: invalid boolean for {key}={val}, ignoring");
                None
            }
        }
    }

    fn read_usize(key: &str) -> Option<usize> {
        let val = std::env::var(key).ok()?;
        match val.parse::<usize>() {
            Ok(n) if n >= 1 => Some(n),
            _ => {
                eprintln!("  warning: invalid positive integer for {key}={val}, ignoring");
                None
            }
        }
    }
}

fn global_rc_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ruckuprc"))
}

/// Load the fully-resolved configuration for a given project directory.
///
/// Layers (later wins):
/// 1. defaults
/// 2. ~/.ruckuprc
/// 3. <dir>/.ruckuprc
/// 4. RUCKUP_* env vars
pub fn load(dir: &Path) -> Config {
    let mut config = Config::default();

    // Global rc
    if let Some(path) = global_rc_path() {
        if let Some(rc) = RcFile::load(&path) {
            rc.apply_to(&mut config);
        }
    }

    // Project-local rc
    if let Some(rc) = RcFile::load(&dir.join(".ruckuprc")) {
        rc.apply_to(&mut config);
    }

    // Environment variables (highest priority)
    EnvLayer::apply_to(&mut config);

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_preserves_range() {
        let config = Config::default();
        assert!(config.preserve_range);
    }

    #[test]
    fn rc_file_toml_parse() {
        let content = r#"preserve_range = false"#;
        let rc: RcFile = toml::from_str(content).unwrap();
        assert_eq!(rc.preserve_range, Some(false));
    }

    #[test]
    fn rc_file_json_parse() {
        let content = r#"{"preserve_range": false}"#;
        let rc: RcFile = serde_json::from_str(content).unwrap();
        assert_eq!(rc.preserve_range, Some(false));
    }

    #[test]
    fn partial_rc_file_leaves_defaults() {
        let content = r#"{}"#;
        let rc: RcFile = serde_json::from_str(content).unwrap();
        let mut config = Config::default();
        rc.apply_to(&mut config);
        assert!(config.preserve_range); // stays default
    }
}
