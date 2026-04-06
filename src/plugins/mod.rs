mod cargo;
mod npm;
mod uv;

use crate::manifest::RawPackage;
use crate::tasks::{ResolvedTask, TaskCommand, TaskRegistry};
use anyhow::{Context, Result};
use glob::glob;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(crate) use cargo::CargoPlugin;
pub(crate) use npm::PackageJsonPlugin;
pub(crate) use uv::UvPlugin;

pub(crate) trait ManifestPlugin {
    fn discover_manifests(&self, root: &Path) -> Result<Vec<PathBuf>>;
    fn parse_manifest(&self, path: &Path) -> Result<RawPackage>;
}

pub(crate) fn default_plugins() -> Vec<Box<dyn ManifestPlugin>> {
    vec![
        Box::new(CargoPlugin),
        Box::new(PackageJsonPlugin),
        Box::new(UvPlugin),
    ]
}

pub(crate) fn batch_execution_units(
    group: Vec<ResolvedTask>,
    tasks: &TaskRegistry,
    root: &Path,
) -> Result<Vec<ExecutionUnit>> {
    let mut units = Vec::new();
    let mut cargo_group = Vec::new();

    for resolved in group {
        match resolved.ecosystem {
            crate::manifest::Ecosystem::Cargo => cargo_group.push(resolved),
            _ => units.push(ExecutionUnit::Single(resolved)),
        }
    }

    units.extend(cargo::batch_execution_units(cargo_group, tasks, root)?);
    units.sort_by_key(|left| left.sort_key());
    Ok(units)
}

pub(crate) enum ExecutionUnit {
    Single(ResolvedTask),
    Batch(BatchedExecution),
}

pub(crate) struct BatchedExecution {
    pub(crate) task_name: String,
    pub(crate) command: TaskCommand,
    pub(crate) package_names: Vec<String>,
    pub(crate) explicitly_opted_in: bool,
    pub(crate) display_label: String,
    pub(crate) working_dir: PathBuf,
}

impl ExecutionUnit {
    pub(crate) fn task_count(&self) -> usize {
        match self {
            ExecutionUnit::Single(_) => 1,
            ExecutionUnit::Batch(batch) => batch.package_names.len(),
        }
    }

    pub(crate) fn display_label(&self, use_color: bool) -> String {
        match self {
            ExecutionUnit::Single(resolved) => resolved.render_colored(use_color),
            ExecutionUnit::Batch(batch) => {
                if use_color {
                    batch.display_label.clone()
                } else {
                    strip_ansi(&batch.display_label)
                }
            }
        }
    }

    pub(crate) fn sort_key(&self) -> String {
        match self {
            ExecutionUnit::Single(resolved) => format!("single:{}", resolved.package_name),
            ExecutionUnit::Batch(batch) => format!("batch:{}", batch.package_names.join(",")),
        }
    }
}

fn strip_ansi(value: &str) -> String {
    let mut result = String::new();
    let mut chars = value.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        result.push(ch);
    }

    result
}

pub(crate) fn expand_workspace_members(
    root: &Path,
    includes: &[String],
    excludes: &[String],
    manifest_name: &str,
) -> Result<Vec<PathBuf>> {
    let mut manifests = BTreeSet::new();
    let excluded = expand_patterns(root, excludes)?;

    for path in expand_patterns(root, includes)? {
        let manifest = if path.is_dir() {
            path.join(manifest_name)
        } else {
            path
        };
        if is_ignored_path(&manifest) {
            continue;
        }
        if manifest.file_name().and_then(|name| name.to_str()) != Some(manifest_name) {
            continue;
        }
        if manifest.exists() && !excluded.contains(&manifest) {
            manifests.insert(manifest);
        }
    }

    Ok(manifests.into_iter().collect())
}

fn expand_patterns(root: &Path, patterns: &[String]) -> Result<BTreeSet<PathBuf>> {
    let mut matches = BTreeSet::new();
    for pattern in patterns {
        let joined = root.join(pattern);
        let pattern = joined.to_string_lossy().to_string();
        for entry in
            glob(&pattern).with_context(|| format!("invalid workspace pattern `{pattern}`"))?
        {
            let path =
                entry.with_context(|| format!("failed to expand workspace pattern `{pattern}`"))?;
            if !is_ignored_path(&path) {
                matches.insert(path);
            }
        }
    }
    Ok(matches)
}

fn is_ignored_path(path: &Path) -> bool {
    path.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|part| {
            matches!(part, ".git" | "target" | "node_modules" | ".venv" | "dist")
        })
    })
}
