mod channels;
mod cli;
mod git;
mod graph;
mod manifest;
mod plugins;
mod stamp;
mod tasks;
mod version;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use graph::WorkspaceGraph;
use manifest::discover_workspace;
use plugins::{ExecutionUnit, batch_execution_units};
use std::io::IsTerminal;
use tasks::{TaskCommand, TaskRegistry};

fn main() -> Result<()> {
    let cli = Cli::parse_from(normalize_args(std::env::args_os()));
    let root = cli.root.canonicalize()?;
    let discovery = discover_workspace(&root)?;
    let stdout_is_terminal = std::io::stdout().is_terminal();
    let use_color = std::io::stderr().is_terminal();
    if !discovery.warnings.is_empty() {
        eprintln!();
        for warning in &discovery.warnings {
            if use_color {
                eprintln!("\x1b[33m{warning}\x1b[0m");
            } else {
                eprintln!("{warning}");
            }
        }
        eprintln!();
    }
    match cli.command {
        Command::Version { channel } => {
            let version_str = calculate_version(&root, channel)?;
            println!("{}", version_str);
        }
        Command::Stamp {
            version: explicit_version,
        } => {
            let version_str = match explicit_version {
                Some(v) => v,
                None => calculate_version(&root, None)?,
            };
            let modified = stamp::stamp_all(&root, &discovery.packages, &version_str)?;
            for path in &modified {
                eprintln!("{}", path);
            }
            println!("{}", version_str);
        }
        command => {
            let graph = WorkspaceGraph::new(discovery.packages);
            match command {
                Command::Graph => {
                    println!("{}", graph.render_tree()?);
                }
                Command::Topo => {
                    for package in graph.topological_order()? {
                        println!(
                            "{} [{}] {}",
                            package.name,
                            package.display_label(),
                            package.manifest_path.display()
                        );
                    }
                }
                Command::Plan { task, ordered } => {
                    let tasks = TaskRegistry::load(&root)?;
                    if ordered {
                        for (index, resolved) in
                            graph.task_plan(&tasks, &task)?.into_iter().enumerate()
                        {
                            println!(
                                "{}. {}",
                                index + 1,
                                resolved.render_colored(stdout_is_terminal)
                            );
                        }
                    } else {
                        println!("{}", graph.render_task_plan_tree(&tasks, &task)?);
                    }
                }
                Command::Run { task } => {
                    let tasks = TaskRegistry::load(&root)?;
                    let groups = graph.task_ready_groups(&tasks, &task)?;
                    let total_tasks = groups.iter().map(Vec::len).sum::<usize>();
                    let mut started = 0usize;
                    for group in groups {
                        for unit in batch_execution_units(group, &tasks, &root)? {
                            let count = unit.task_count();
                            started += count;
                            println!(
                                "{}",
                                render_run_start(
                                    &unit.display_label(stdout_is_terminal),
                                    started + 1 - count,
                                    started,
                                    total_tasks,
                                    stdout_is_terminal
                                )
                            );
                            if let Err(error) = execute_unit(&unit, &root) {
                                handle_unit_failure(&unit, error, use_color)?;
                            }
                        }
                    }
                }
                Command::Version { .. } | Command::Stamp { .. } => unreachable!(),
            }
        }
    }

    Ok(())
}

fn calculate_version(root: &std::path::Path, channel_override: Option<String>) -> Result<String> {
    let tasks = TaskRegistry::load(root)?;
    let channels_table = tasks.channels();

    let channel_config = if let Some(override_channel) = channel_override {
        channels::ChannelConfig {
            channel: override_channel.clone(),
            prerelease: override_channel != "production",
        }
    } else {
        let branch = git::get_current_branch()?;
        let channels_map = channels_table
            .map(channels::parse_channels)
            .unwrap_or_default();
        channels::resolve_channel(&branch, &channels_map).ok_or_else(|| {
            anyhow::anyhow!(
                "branch '{}' is not mapped to a release channel in flux.toml [channels]",
                branch
            )
        })?
    };

    let latest_tag = git::get_latest_production_tag();
    let commits = git::get_commits_since(latest_tag.as_deref());

    anyhow::ensure!(!commits.is_empty(), "no commits since last production tag");

    let current = latest_tag
        .as_ref()
        .map(|t| version::Version::parse(t))
        .unwrap_or(version::Version {
            major: 0,
            minor: 0,
            patch: 0,
        });

    let next = version::calculate_next_version(current, &commits);
    let base = next.format();

    let full_version = if channel_config.prerelease {
        let count = git::get_existing_prerelease_count(&base, &channel_config.channel) + 1;
        format!("{}-{}.{}", base, channel_config.channel, count)
    } else {
        base
    };

    Ok(full_version)
}

fn execute_task(
    command: &TaskCommand,
    cwd: &std::path::Path,
    variables: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let status = match command {
        TaskCommand::Shell(command) => {
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-lc").arg(command).current_dir(cwd).envs(variables);
            cmd.status()?
        }
        TaskCommand::Argv(command) => {
            let (program, args) = command
                .split_first()
                .ok_or_else(|| anyhow::anyhow!("resolved task command was empty"))?;
            let mut cmd = std::process::Command::new(program);
            cmd.args(args).current_dir(cwd).envs(variables);
            cmd.status()?
        }
    };
    anyhow::ensure!(
        status.success(),
        "task execution failed in {}",
        cwd.display()
    );
    Ok(())
}

fn execute_unit(unit: &ExecutionUnit, _root: &std::path::Path) -> Result<()> {
    match unit {
        ExecutionUnit::Single(resolved) => {
            execute_task(&resolved.command, &resolved.package_dir, &resolved.variables)
        }
        ExecutionUnit::Batch(batch) => {
            let status = match &batch.command {
                TaskCommand::Shell(command) => std::process::Command::new("sh")
                    .arg("-lc")
                    .arg(command)
                    .current_dir(&batch.working_dir)
                    .status()?,
                TaskCommand::Argv(command) => {
                    let (program, args) = command
                        .split_first()
                        .ok_or_else(|| anyhow::anyhow!("batched task command was empty"))?;
                    std::process::Command::new(program)
                        .args(args)
                        .current_dir(&batch.working_dir)
                        .status()?
                }
            };
            anyhow::ensure!(
                status.success(),
                "batched task execution failed in {}",
                batch.working_dir.display()
            );
            Ok(())
        }
    }
}

fn handle_task_failure(
    resolved: &tasks::ResolvedTask,
    error: anyhow::Error,
    use_color: bool,
) -> Result<()> {
    if resolved.explicitly_opted_in {
        return Err(error);
    }

    let message = format!(
        "warning: autoapplied task `{}` failed for `{}` and will be treated as a no-op: {}",
        resolved.task_name, resolved.package_name, error
    );
    emit_warning(&message, use_color);
    Ok(())
}

fn handle_unit_failure(unit: &ExecutionUnit, error: anyhow::Error, use_color: bool) -> Result<()> {
    match unit {
        ExecutionUnit::Single(resolved) => handle_task_failure(resolved, error, use_color),
        ExecutionUnit::Batch(batch) => {
            if batch.explicitly_opted_in {
                return Err(error);
            }
            let message = format!(
                "warning: autoapplied batched task `{}` failed for `{}` and will be treated as a no-op: {}",
                batch.task_name,
                batch.package_names.join(", "),
                error
            );
            emit_warning(&message, use_color);
            Ok(())
        }
    }
}

fn emit_warning(message: &str, use_color: bool) {
    if use_color {
        eprintln!("\x1b[33m{message}\x1b[0m");
    } else {
        eprintln!("{message}");
    }
}

fn render_run_start(
    label: &str,
    start: usize,
    end: usize,
    total: usize,
    use_color: bool,
) -> String {
    let progress = if start == end {
        format!("[{start}/{total}]")
    } else {
        format!("[{start}-{end}/{total}]")
    };
    let prefix = if use_color {
        format!("\x1b[2m{progress}\x1b[0m ")
    } else {
        format!("{progress} ")
    };
    format!("{prefix}{label}")
}

fn normalize_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut args = args.into_iter().collect::<Vec<_>>();
    if args.get(1).is_some_and(|arg| arg == "flux") {
        args.remove(1);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::{handle_task_failure, normalize_args};
    use crate::cli::{Cli, Command};
    use crate::manifest::{Ecosystem, Package, PackageId};
    use crate::plugins::{ExecutionUnit, batch_execution_units};
    use crate::tasks::{ResolvedTask, TaskCommand, TaskRegistry};
    use clap::Parser;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn strips_cargo_forwarded_subcommand_name() {
        let args = vec![
            OsString::from("cargo-flux"),
            OsString::from("flux"),
            OsString::from("graph"),
        ];

        let actual = normalize_args(args);
        assert_eq!(
            actual,
            vec![OsString::from("cargo-flux"), OsString::from("graph")]
        );
    }

    #[test]
    fn leaves_direct_binary_invocation_unchanged() {
        let args = vec![OsString::from("cargo-flux"), OsString::from("graph")];

        let actual = normalize_args(args.clone());
        assert_eq!(actual, args);
    }

    #[test]
    fn autoapplied_task_failures_are_downgraded_to_warnings() {
        let resolved = ResolvedTask {
            task_name: "build".to_string(),
            package_name: "shared".to_string(),
            ecosystem: Ecosystem::Cargo,
            display_label: "cargo".to_string(),
            explicitly_opted_in: false,
            package_dir: PathBuf::from("."),
            variables: BTreeMap::new(),
            command: TaskCommand::Argv(vec!["false".to_string()]),
        };

        let result = handle_task_failure(&resolved, anyhow::anyhow!("boom"), false);
        assert!(result.is_ok());
    }

    #[test]
    fn explicit_task_failures_still_abort() {
        let resolved = ResolvedTask {
            task_name: "build".to_string(),
            package_name: "app".to_string(),
            ecosystem: Ecosystem::Cargo,
            display_label: "cargo".to_string(),
            explicitly_opted_in: true,
            package_dir: PathBuf::from("."),
            variables: BTreeMap::new(),
            command: TaskCommand::Argv(vec!["false".to_string()]),
        };

        let result = handle_task_failure(&resolved, anyhow::anyhow!("boom"), false);
        assert!(result.is_err());
    }

    #[test]
    fn parses_ordered_plan_flag() {
        let cli = Cli::parse_from(["cargo-flux", "plan", "build", "--ordered"]);

        match cli.command {
            Command::Plan { task, ordered } => {
                assert_eq!(task, "build");
                assert!(ordered);
            }
            other => panic!("expected plan command, got {other:?}"),
        }
    }

    #[test]
    fn batches_workspace_batchable_cargo_tasks() {
        let root = temp_dir("batchable-cargo-run");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.check]
workspace_batchable = true
cargo = ["cargo", "check"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let a = cargo_resolved_task("a", true);
        let b = cargo_resolved_task("b", false);

        let units = batch_execution_units(vec![a, b], &registry, &root).expect("batch units");
        assert_eq!(units.len(), 1);
        match &units[0] {
            ExecutionUnit::Batch(batch) => {
                let TaskCommand::Argv(command) = &batch.command else {
                    panic!("expected argv command");
                };
                assert_eq!(batch.task_name, "check");
                assert_eq!(batch.package_names, vec!["a".to_string(), "b".to_string()]);
                assert!(batch.explicitly_opted_in);
                assert_eq!(
                    command,
                    &vec![
                        "cargo".to_string(),
                        "check".to_string(),
                        "-p".to_string(),
                        "a".to_string(),
                        "-p".to_string(),
                        "b".to_string(),
                    ]
                );
            }
            other => panic!("expected cargo batch, got {}", other.display_label(false)),
        }
    }

    fn cargo_resolved_task(package_name: &str, explicitly_opted_in: bool) -> ResolvedTask {
        let package = Package {
            id: PackageId::new(Ecosystem::Cargo, package_name),
            name: package_name.to_string(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: PathBuf::from(format!("{package_name}/Cargo.toml")),
            js_package_manager: None,
            task_opt_ins: Default::default(),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![],
        };

        ResolvedTask {
            task_name: "check".to_string(),
            package_name: package.name,
            ecosystem: Ecosystem::Cargo,
            display_label: "cargo".to_string(),
            explicitly_opted_in,
            package_dir: PathBuf::from(package_name),
            variables: BTreeMap::new(),
            command: TaskCommand::Argv(vec!["cargo".to_string(), "check".to_string()]),
        }
    }

    #[test]
    fn parses_version_command() {
        let cli = Cli::parse_from(["cargo-flux", "version"]);
        match cli.command {
            Command::Version { channel } => {
                assert!(channel.is_none());
            }
            other => panic!("expected version command, got {other:?}"),
        }
    }

    #[test]
    fn parses_version_command_with_channel_override() {
        let cli = Cli::parse_from(["cargo-flux", "version", "--channel", "beta"]);
        match cli.command {
            Command::Version { channel } => {
                assert_eq!(channel.as_deref(), Some("beta"));
            }
            other => panic!("expected version command, got {other:?}"),
        }
    }

    #[test]
    fn parses_stamp_command_with_explicit_version() {
        let cli = Cli::parse_from(["cargo-flux", "stamp", "1.2.3"]);
        match cli.command {
            Command::Stamp { version } => {
                assert_eq!(version.as_deref(), Some("1.2.3"));
            }
            other => panic!("expected stamp command, got {other:?}"),
        }
    }

    #[test]
    fn parses_stamp_command_without_version() {
        let cli = Cli::parse_from(["cargo-flux", "stamp"]);
        match cli.command {
            Command::Stamp { version } => {
                assert!(version.is_none());
            }
            other => panic!("expected stamp command, got {other:?}"),
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should work")
            .as_millis();
        let path = std::env::temp_dir().join(format!("cargo-flux-main-{prefix}-{millis}"));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }
}
