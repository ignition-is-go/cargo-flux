use crate::manifest::{
    BridgeTarget, Ecosystem, PackageId, RawPackage, TaskOptIn, WarnedDependencyRef,
};
use crate::plugins::{BatchedExecution, ExecutionUnit, ManifestPlugin, expand_workspace_members};
use crate::tasks::{ResolvedTask, TaskCommand, TaskRegistry};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) struct CargoPlugin;

impl ManifestPlugin for CargoPlugin {
    fn discover_manifests(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let manifest_path = root.join("Cargo.toml");
        if !manifest_path.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&manifest_path)
            .with_context(|| format!("failed to read manifest {}", manifest_path.display()))?;
        let manifest = toml::from_str::<CargoManifest>(&content).with_context(|| {
            format!("failed to parse cargo manifest {}", manifest_path.display())
        })?;

        let mut manifests = Vec::new();
        if manifest.package.is_some() {
            manifests.push(manifest_path.clone());
        }

        if let Some(workspace) = manifest.workspace {
            manifests.extend(expand_workspace_members(
                root,
                &workspace.members.unwrap_or_default(),
                &workspace.exclude.unwrap_or_default(),
                "Cargo.toml",
            )?);
        }

        manifests.sort();
        manifests.dedup();
        Ok(manifests)
    }

    fn parse_manifest(&self, path: &Path) -> Result<RawPackage> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read manifest {}", path.display()))?;
        let manifest = toml::from_str::<CargoManifest>(&content)
            .with_context(|| format!("failed to parse cargo manifest {}", path.display()))?;
        let package = match manifest.package {
            Some(package) => package,
            None => {
                let fallback_name = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("workspace");
                CargoPackage {
                    name: fallback_name.to_string(),
                    metadata: None,
                }
            }
        };

        let build_dependencies = manifest.build_dependencies.clone().unwrap_or_default();
        let mut deps = local_cargo_dependencies(manifest.dependencies.unwrap_or_default());
        deps.extend(local_cargo_dependencies(build_dependencies.clone()));
        deps.sort();
        deps.dedup();
        let mut warned_deps =
            local_cargo_dependencies(manifest.dev_dependencies.unwrap_or_default())
                .into_iter()
                .map(|name| WarnedDependencyRef {
                    name,
                    section: "dev-dependencies".to_string(),
                })
                .collect::<Vec<_>>();
        warned_deps.extend(
            local_cargo_dependencies(build_dependencies)
                .into_iter()
                .map(|name| WarnedDependencyRef {
                    name,
                    section: "build-dependencies".to_string(),
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
            if let Some(flux) = package.metadata.and_then(|metadata| metadata.flux) {
                (parse_task_opt_ins(flux.tasks), parse_bridges(flux.bridges)?)
            } else {
                Default::default()
            };

        Ok(RawPackage {
            id: PackageId::new(Ecosystem::Cargo, &package.name),
            name: package.name,
            ecosystem: Ecosystem::Cargo,
            manifest_path: path.to_path_buf(),
            js_package_manager: None,
            task_opt_ins,
            bridged_dependencies,
            declared_dependencies: deps,
            warned_declared_dependencies: warned_deps,
        })
    }
}

fn local_cargo_dependencies(dependencies: BTreeMap<String, toml::Value>) -> Vec<String> {
    dependencies
        .into_iter()
        .filter_map(|(name, value)| is_local_cargo_dependency(&value).then_some(name))
        .collect()
}

fn is_local_cargo_dependency(value: &toml::Value) -> bool {
    let Some(table) = value.as_table() else {
        return false;
    };

    table.contains_key("path")
        || table
            .get("workspace")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false)
}

#[derive(Debug, Deserialize)]
struct CargoManifest {
    package: Option<CargoPackage>,
    workspace: Option<CargoWorkspace>,
    dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "build-dependencies")]
    build_dependencies: Option<BTreeMap<String, toml::Value>>,
}

#[derive(Debug, Deserialize)]
struct CargoWorkspace {
    members: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    metadata: Option<CargoPackageMetadata>,
}

#[derive(Debug, Deserialize)]
struct CargoPackageMetadata {
    flux: Option<FluxManifestConfig>,
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
            .collect::<BTreeSet<_>>()
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

pub(crate) fn batch_execution_units(
    group: Vec<ResolvedTask>,
    tasks: &TaskRegistry,
    root: &Path,
) -> Result<Vec<ExecutionUnit>> {
    let mut units = Vec::new();
    let mut pending_batches = BTreeMap::<(String, Vec<String>), Vec<ResolvedTask>>::new();

    for resolved in group {
        if tasks.workspace_batchable(&resolved.task_name)?
            && let Some(command) = cargo_batch_command(&resolved.command)
        {
            pending_batches
                .entry((resolved.task_name.clone(), command))
                .or_default()
                .push(resolved);
        } else {
            units.push(ExecutionUnit::Single(resolved));
        }
    }

    for ((task_name, command), mut resolved_group) in pending_batches {
        resolved_group.sort_by(|left, right| left.package_name.cmp(&right.package_name));
        if resolved_group.len() == 1 {
            units.push(ExecutionUnit::Single(
                resolved_group
                    .into_iter()
                    .next()
                    .expect("single resolved task"),
            ));
            continue;
        }

        let package_names = resolved_group
            .iter()
            .map(|resolved| resolved.package_name.clone())
            .collect::<Vec<_>>();
        let explicitly_opted_in = resolved_group
            .iter()
            .any(|resolved| resolved.explicitly_opted_in);

        units.push(ExecutionUnit::Batch(BatchedExecution {
            task_name: task_name.clone(),
            command: TaskCommand::Argv(batch_command_for_packages(command, &package_names)),
            package_names: package_names.clone(),
            explicitly_opted_in,
            display_label: render_batch_label(&task_name, &package_names, true),
            working_dir: root.to_path_buf(),
        }));
    }

    Ok(units)
}

fn cargo_batch_command(command: &TaskCommand) -> Option<Vec<String>> {
    let TaskCommand::Argv(argv) = command else {
        return None;
    };
    let (program, args) = argv.split_first()?;
    (program == "cargo").then(|| {
        let mut command = vec![program.clone()];
        command.extend_from_slice(args);
        command
    })
}

fn batch_command_for_packages(mut command: Vec<String>, package_names: &[String]) -> Vec<String> {
    for package_name in package_names {
        command.push("-p".to_string());
        command.push(package_name.clone());
    }
    command
}

fn render_batch_label(task_name: &str, package_names: &[String], use_color: bool) -> String {
    let packages = package_names.join(", ");
    if use_color {
        format!(
            "\x1b[1mcargo-batch\x1b[0m:\x1b[1;97m{task_name}\x1b[0m [\x1b[1;38;5;208mcargo\x1b[0m] \x1b[2m{{{packages}}}\x1b[0m"
        )
    } else {
        format!("cargo-batch:{task_name} [cargo] {{{packages}}}")
    }
}
