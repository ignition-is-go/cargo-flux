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
