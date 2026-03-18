use crate::manifest::{
    BridgeTarget, Ecosystem, JsPackageManager, PackageId, RawPackage, TaskOptIn,
    WarnedDependencyRef,
};
use crate::plugins::{ManifestPlugin, expand_workspace_members};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct PackageJsonPlugin;

impl ManifestPlugin for PackageJsonPlugin {
    fn discover_manifests(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let manifest_path = root.join("package.json");
        let pnpm_workspace_path = root.join("pnpm-workspace.yaml");
        if !manifest_path.exists() && !pnpm_workspace_path.exists() {
            return Ok(Vec::new());
        }

        let mut manifests = Vec::new();
        let mut include_patterns = Vec::new();
        let mut exclude_patterns = Vec::new();

        if manifest_path.exists() {
            let content = fs::read_to_string(&manifest_path)
                .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
            let manifest =
                serde_json::from_str::<PackageJsonManifest>(&content).with_context(|| {
                    format!("failed to parse package.json {}", manifest_path.display())
                })?;

            manifests.push(manifest_path.clone());
            if let Some(workspaces) = manifest.workspaces {
                let (includes, excludes) = workspaces.into_patterns();
                include_patterns.extend(includes);
                exclude_patterns.extend(excludes);
            }
        }

        if pnpm_workspace_path.exists() {
            let content = fs::read_to_string(&pnpm_workspace_path).with_context(|| {
                format!("failed to read manifest {}", pnpm_workspace_path.display())
            })?;
            let workspace = serde_yaml::from_str::<PnpmWorkspaceManifest>(&content)
                .with_context(|| format!("failed to parse {}", pnpm_workspace_path.display()))?;
            let (includes, excludes) = split_patterns(workspace.packages.unwrap_or_default());
            include_patterns.extend(includes);
            exclude_patterns.extend(excludes);
        }

        manifests.extend(expand_workspace_members(
            root,
            &include_patterns,
            &exclude_patterns,
            "package.json",
        )?);

        manifests.sort();
        manifests.dedup();
        Ok(manifests)
    }

    fn parse_manifest(&self, path: &Path) -> Result<RawPackage> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read manifest {}", path.display()))?;
        let manifest = serde_json::from_str::<PackageJsonManifest>(&content)
            .with_context(|| format!("failed to parse package.json {}", path.display()))?;

        let js_package_manager = detect_package_manager(path, &manifest);
        let name = manifest.name.or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        });
        let name = name.unwrap_or_else(|| "package".to_string());
        let (task_opt_ins, bridged_dependencies) = if let Some(flux) = manifest.flux {
            (parse_task_opt_ins(flux.tasks), parse_bridges(flux.bridges)?)
        } else {
            Default::default()
        };

        let mut deps = local_node_dependencies(manifest.dependencies.unwrap_or_default());
        deps.extend(local_node_dependencies(
            manifest.peer_dependencies.unwrap_or_default(),
        ));
        deps.sort();
        deps.dedup();
        let mut warned_deps =
            local_node_dependencies(manifest.dev_dependencies.unwrap_or_default())
                .into_iter()
                .map(|name| WarnedDependencyRef {
                    name,
                    section: "devDependencies".to_string(),
                })
                .collect::<Vec<_>>();
        warned_deps.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then(left.section.cmp(&right.section))
        });
        warned_deps
            .dedup_by(|left, right| left.name == right.name && left.section == right.section);

        Ok(RawPackage {
            id: PackageId::new(Ecosystem::Js, &name),
            name,
            ecosystem: Ecosystem::Js,
            manifest_path: path.to_path_buf(),
            js_package_manager,
            task_opt_ins,
            bridged_dependencies,
            declared_dependencies: deps,
            warned_declared_dependencies: warned_deps,
        })
    }
}

fn local_node_dependencies(dependencies: BTreeMap<String, serde_json::Value>) -> Vec<String> {
    dependencies
        .into_iter()
        .filter_map(|(name, value)| is_local_node_dependency(&value).then_some(name))
        .collect()
}

fn is_local_node_dependency(value: &serde_json::Value) -> bool {
    let Some(spec) = value.as_str() else {
        return false;
    };

    matches!(
        spec,
        s if s.starts_with("workspace:")
            || s.starts_with("file:")
            || s.starts_with("link:")
            || s.starts_with("portal:")
    )
}

fn detect_package_manager(path: &Path, manifest: &PackageJsonManifest) -> Option<JsPackageManager> {
    manifest
        .package_manager
        .as_deref()
        .and_then(parse_package_manager_name)
        .or_else(|| detect_from_lockfiles(path))
        .or(Some(JsPackageManager::Npm))
}

fn parse_package_manager_name(value: &str) -> Option<JsPackageManager> {
    let name = value.split('@').next().unwrap_or("").trim();
    match name {
        "npm" => Some(JsPackageManager::Npm),
        "pnpm" => Some(JsPackageManager::Pnpm),
        "yarn" => Some(JsPackageManager::Yarn),
        "bun" => Some(JsPackageManager::Bun),
        _ => None,
    }
}

fn detect_from_lockfiles(path: &Path) -> Option<JsPackageManager> {
    for dir in ancestors(path.parent()?) {
        if dir.join("pnpm-lock.yaml").exists() {
            return Some(JsPackageManager::Pnpm);
        }
        if dir.join("yarn.lock").exists() {
            return Some(JsPackageManager::Yarn);
        }
        if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            return Some(JsPackageManager::Bun);
        }
        if dir.join("package-lock.json").exists() {
            return Some(JsPackageManager::Npm);
        }
    }
    None
}

fn ancestors(start: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    start.ancestors().map(Path::to_path_buf)
}

#[derive(Debug, Deserialize)]
struct PackageJsonManifest {
    name: Option<String>,
    #[serde(rename = "packageManager")]
    package_manager: Option<String>,
    flux: Option<FluxManifestConfig>,
    workspaces: Option<WorkspacesField>,
    dependencies: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(rename = "devDependencies")]
    dev_dependencies: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(rename = "peerDependencies")]
    peer_dependencies: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct FluxManifestConfig {
    #[serde(default)]
    tasks: TaskOptInsField,
    #[serde(default)]
    bridges: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
enum TaskOptInsField {
    #[default]
    Empty,
    List(Vec<String>),
    Map(BTreeMap<String, TaskOptInConfig>),
}

#[derive(Debug, Deserialize, Default)]
struct TaskOptInConfig {
    #[serde(default)]
    variables: BTreeMap<String, String>,
}

fn parse_task_opt_ins(tasks: TaskOptInsField) -> BTreeMap<String, TaskOptIn> {
    match tasks {
        TaskOptInsField::Empty => BTreeMap::new(),
        TaskOptInsField::List(names) => names
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|name| (name, TaskOptIn::default()))
            .collect(),
        TaskOptInsField::Map(entries) => entries
            .into_iter()
            .map(|(name, config)| {
                (
                    name,
                    TaskOptIn {
                        variables: config.variables,
                    },
                )
            })
            .collect(),
    }
}

fn parse_bridges(bridges: Vec<String>) -> Result<Vec<BridgeTarget>> {
    bridges
        .into_iter()
        .map(|bridge| BridgeTarget::parse(&bridge))
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkspacesField {
    Array(Vec<String>),
    Object(WorkspacesObject),
}

impl WorkspacesField {
    fn into_patterns(self) -> (Vec<String>, Vec<String>) {
        match self {
            WorkspacesField::Array(patterns) => split_patterns(patterns),
            WorkspacesField::Object(object) => split_patterns(object.packages.unwrap_or_default()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkspacesObject {
    packages: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct PnpmWorkspaceManifest {
    packages: Option<Vec<String>>,
}

fn split_patterns(patterns: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    for pattern in patterns {
        if let Some(stripped) = pattern.strip_prefix('!') {
            excludes.push(stripped.to_string());
        } else {
            includes.push(pattern);
        }
    }
    (includes, excludes)
}
