use crate::manifest::{
    BridgeTarget, Ecosystem, PackageId, RawPackage, TaskOptIn, WarnedDependencyRef,
};
use crate::plugins::{ManifestPlugin, expand_workspace_members};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct UvPlugin;

impl ManifestPlugin for UvPlugin {
    fn discover_manifests(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let manifest_path = root.join("pyproject.toml");
        if !manifest_path.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&manifest_path)
            .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
        let manifest = toml::from_str::<PyProjectManifest>(&content)
            .with_context(|| format!("failed to parse pyproject {}", manifest_path.display()))?;

        let mut manifests = vec![manifest_path];
        if let Some(workspace) = manifest
            .tool
            .as_ref()
            .and_then(|tool| tool.uv.as_ref())
            .and_then(|uv| uv.workspace.as_ref())
        {
            manifests.extend(expand_workspace_members(
                root,
                &workspace.members.clone().unwrap_or_default(),
                &workspace.exclude.clone().unwrap_or_default(),
                "pyproject.toml",
            )?);
        }

        manifests.sort();
        manifests.dedup();
        Ok(manifests)
    }

    fn parse_manifest(&self, path: &Path) -> Result<RawPackage> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read manifest {}", path.display()))?;
        let manifest = toml::from_str::<PyProjectManifest>(&content)
            .with_context(|| format!("failed to parse pyproject {}", path.display()))?;

        let name = manifest
            .project
            .as_ref()
            .and_then(|project| project.name.clone())
            .or_else(|| {
                manifest
                    .tool
                    .as_ref()
                    .and_then(|tool| tool.uv.as_ref())
                    .and_then(|uv| uv.package.clone())
            })
            .or_else(|| {
                path.parent()
                    .and_then(|p| p.file_name())
                    .and_then(|name| name.to_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| "python-package".to_string());

        let mut deps = manifest
            .project
            .as_ref()
            .and_then(|project| project.dependencies.clone())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|spec| normalize_python_dependency(&spec))
            .filter(|dep| is_local_uv_dependency(&manifest, dep))
            .collect::<Vec<_>>();

        if let Some(optional) = manifest
            .project
            .as_ref()
            .and_then(|project| project.optional_dependencies.clone())
        {
            deps.extend(
                optional
                    .into_values()
                    .flatten()
                    .filter_map(|spec| normalize_python_dependency(&spec))
                    .filter(|dep| is_local_uv_dependency(&manifest, dep)),
            );
        }

        deps.sort();
        deps.dedup();
        let dependency_groups = manifest.dependency_groups.clone().unwrap_or_default();
        let uv_dev_dependencies = manifest
            .tool
            .as_ref()
            .and_then(|tool| tool.uv.as_ref())
            .and_then(|uv| uv.dev_dependencies.clone())
            .unwrap_or_default();
        let manifest_ref = &manifest;
        let mut warned_deps = dependency_groups
            .into_iter()
            .flat_map(|(group, specs)| {
                let section = format!("dependency-groups.{group}");
                specs.into_iter().filter_map(move |spec| {
                    normalize_python_dependency(&spec)
                        .filter(|dep| is_local_uv_dependency(manifest_ref, dep))
                        .map(|name| WarnedDependencyRef {
                            name,
                            section: section.clone(),
                        })
                })
            })
            .collect::<Vec<_>>();
        warned_deps.extend(
            uv_dev_dependencies
                .into_iter()
                .filter_map(|spec| normalize_python_dependency(&spec))
                .filter(|dep| is_local_uv_dependency(manifest_ref, dep))
                .map(|name| WarnedDependencyRef {
                    name,
                    section: "tool.uv.dev-dependencies".to_string(),
                }),
        );
        warned_deps.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then(left.section.cmp(&right.section))
        });
        warned_deps
            .dedup_by(|left, right| left.name == right.name && left.section == right.section);
        let (task_opt_ins, bridged_dependencies) =
            if let Some(flux) = manifest.tool.and_then(|tool| tool.flux) {
                (parse_task_opt_ins(flux.tasks), parse_bridges(flux.bridges)?)
            } else {
                Default::default()
            };

        Ok(RawPackage {
            id: PackageId::new(Ecosystem::Uv, &name),
            name,
            ecosystem: Ecosystem::Uv,
            manifest_path: path.to_path_buf(),
            js_package_manager: None,
            task_opt_ins,
            bridged_dependencies,
            declared_dependencies: deps,
            warned_declared_dependencies: warned_deps,
        })
    }
}

fn is_local_uv_dependency(manifest: &PyProjectManifest, name: &str) -> bool {
    manifest
        .tool
        .as_ref()
        .and_then(|tool| tool.uv.as_ref())
        .and_then(|uv| uv.sources.as_ref())
        .and_then(|sources| sources.get(name))
        .is_some_and(UvSourceSpec::is_local)
}

fn normalize_python_dependency(spec: &str) -> Option<String> {
    let first = spec
        .split([' ', '[', ';', '<', '>', '=', '!', '~'])
        .next()
        .unwrap_or("")
        .trim();
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

#[derive(Debug, Deserialize)]
struct PyProjectManifest {
    project: Option<PythonProject>,
    tool: Option<PythonTool>,
    #[serde(rename = "dependency-groups")]
    dependency_groups: Option<BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Deserialize)]
struct PythonProject {
    name: Option<String>,
    dependencies: Option<Vec<String>>,
    #[serde(rename = "optional-dependencies")]
    optional_dependencies: Option<BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Deserialize)]
struct PythonTool {
    uv: Option<UvTool>,
    flux: Option<FluxManifestConfig>,
}

#[derive(Debug, Deserialize)]
struct UvTool {
    package: Option<String>,
    workspace: Option<UvWorkspace>,
    sources: Option<BTreeMap<String, UvSourceSpec>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UvSourceSpec {
    Single(UvSource),
    Multiple(Vec<UvSource>),
}

impl UvSourceSpec {
    fn is_local(&self) -> bool {
        match self {
            UvSourceSpec::Single(source) => source.is_local(),
            UvSourceSpec::Multiple(sources) => sources.iter().any(UvSource::is_local),
        }
    }
}

#[derive(Debug, Deserialize)]
struct UvSource {
    path: Option<String>,
    workspace: Option<bool>,
}

impl UvSource {
    fn is_local(&self) -> bool {
        self.path.is_some() || self.workspace.unwrap_or(false)
    }
}

#[derive(Debug, Deserialize)]
struct UvWorkspace {
    members: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
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
