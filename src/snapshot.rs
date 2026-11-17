//! Language-independent contract between cache providers and archive storage.
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Preparation runs before enumeration, allowing a provider to hold coordination
/// guards throughout enumeration, selection, verification and upload.
pub(crate) trait SnapshotPolicy {
    fn prepare(&self, root: &Path, paths: &[PathBuf]) -> Result<Box<dyn SnapshotSelection>>;
}

pub(crate) trait SnapshotSelection {
    /// Select only from the declared candidate files. Storage enforces this bound.
    fn select(&mut self, candidates: &BTreeSet<PathBuf>) -> Result<SnapshotPlan>;
}

pub(crate) struct SnapshotPlan {
    pub(crate) files: BTreeSet<PathBuf>,
    pub(crate) include_empty_directories: bool,
}

/// Generic snapshot contract. Only the selected plugin interprets `options`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Definition {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) plugin: Option<String>,
    #[serde(default)]
    pub(crate) options: toml::Table,
}

impl Definition {
    pub(crate) fn validate(&self) -> Result<()> {
        crate::storage::validate_paths(&self.paths)?;
        anyhow::ensure!(
            self.plugin.is_some() || self.options.is_empty(),
            "snapshot options require a plugin"
        );
        Ok(())
    }
}

/// CLI options are string lists; manifests can supply any plugin-defined TOML
/// types. Repetition preserves every value, including manifest paths with '='.
pub(crate) fn cli_options(options: &[String]) -> Result<toml::Table> {
    use anyhow::Context;
    let mut table = toml::Table::new();
    for option in options {
        let (key, value) = option
            .split_once('=')
            .context("snapshot options must be KEY=VALUE")?;
        anyhow::ensure!(!key.is_empty(), "snapshot option key must not be empty");
        table
            .entry(key)
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .expect("CLI options are lists")
            .push(toml::Value::String(value.into()));
    }
    Ok(table)
}
