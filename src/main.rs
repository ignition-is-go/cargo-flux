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
use plugins::{ExecutionUnit, batch_execution_plan};
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
            match calculate_version(&root, channel)? {
                // A release-worthy version goes to stdout, alone, so `$(cargo
                // flux version)` captures exactly it.
                Some(version_str) => println!("{}", version_str),
                // No release-worthy commits: empty stdout, reason on stderr, exit
                // 0. Callers guard on empty stdout; a genuine error still exits
                // non-zero, so empty-and-successful unambiguously means "nothing
                // to release" rather than a swallowed failure.
                None => eprintln!(
                    "no release-worthy commits since the last production tag \
                     (only non-releasing types like docs/chore); nothing to release"
                ),
            }
        }
        Command::Stamp {
            version: explicit_version,
            packages,
            exclude,
            exclude_versions,
        } => {
            let version_str = match explicit_version {
                Some(v) => v,
                None => calculate_version(&root, None)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "no release-worthy commits since the last production tag; \
                         nothing to stamp. Pass an explicit version to override."
                    )
                })?,
            };
            // Reject an empty or malformed version before writing anything. A
            // release workflow that forwards `cargo flux version`'s (now
            // possibly empty) output as `stamp "$VERSION"` without guarding it
            // would otherwise stamp `""` into every manifest and tag `v` — a
            // corrupt release. Fail loudly instead.
            anyhow::ensure!(
                version::Version::is_valid(&version_str),
                "refusing to stamp invalid version {version_str:?}: expected `MAJOR.MINOR.PATCH` \
                 optionally followed by a `-prerelease` suffix"
            );
            let config = stamp::StampConfig::load(&root)?;
            let modified = stamp::stamp_selected(
                &root,
                &discovery.packages,
                &version_str,
                &stamp::StampOptions {
                    packages,
                    exclude,
                    exclude_versions: config
                        .exclude_versions
                        .into_iter()
                        .chain(exclude_versions)
                        .collect(),
                },
            )?;
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
                Command::Plan {
                    task,
                    affected,
                    ordered,
                    stamp_args,
                } => {
                    let tasks = TaskRegistry::load(&root)?;
                    let affected = calculate_affected_scope(&root, &graph, affected.as_deref())?;
                    let affected_packages = affected.as_ref().map(|scope| &scope.packages);
                    let root_plan = if affected.as_ref().is_none_or(|scope| scope.has_changes) {
                        tasks.root_task_plan(&task)?
                    } else {
                        Vec::new()
                    };

                    if stamp_args {
                        let package_names =
                            task_plan_with_scope(&graph, &tasks, &task, affected_packages)?
                                .into_iter()
                                .filter(|resolved| resolved.ecosystem != manifest::Ecosystem::Uv)
                                .map(|resolved| resolved.package_name)
                                .collect::<std::collections::BTreeSet<_>>();
                        for package_name in package_names {
                            println!("--package={package_name}");
                        }
                    } else if ordered {
                        for (index, resolved) in root_plan.iter().enumerate() {
                            println!("{}. {}", index + 1, resolved.render());
                        }
                        let offset = root_plan.len();
                        for (index, resolved) in
                            task_plan_with_scope(&graph, &tasks, &task, affected_packages)?
                                .into_iter()
                                .enumerate()
                        {
                            println!(
                                "{}. {}",
                                offset + index + 1,
                                resolved.render_colored(stdout_is_terminal)
                            );
                        }
                    } else {
                        for resolved in &root_plan {
                            println!("{}", resolved.render());
                        }
                        let rendered =
                            render_task_plan_with_scope(&graph, &tasks, &task, affected_packages)?;
                        if !root_plan.is_empty() && !rendered.is_empty() {
                            println!();
                        }
                        if !rendered.is_empty() {
                            println!("{rendered}");
                        }
                    }
                }
                Command::Affected { base } => {
                    let changed = git::get_changed_files(&root, &base)?;
                    let affected = graph.affected_packages(&root, &changed)?;
                    for package in graph.affected_in_native_order(&affected)? {
                        println!("{}", package.id);
                    }
                }
                Command::Run { task, affected } => {
                    let tasks = TaskRegistry::load(&root)?;
                    let affected_base = affected.clone();
                    let affected = calculate_affected_scope(&root, &graph, affected.as_deref())?;
                    let affected_packages = affected.as_ref().map(|scope| &scope.packages);
                    let root_plan = if affected.as_ref().is_none_or(|scope| scope.has_changes) {
                        tasks.root_task_plan(&task)?
                    } else {
                        Vec::new()
                    };
                    let plan =
                        task_execution_plan_with_scope(&graph, &tasks, &task, affected_packages)?;
                    let total_tasks = root_plan.len() + plan.tasks.len();
                    if total_tasks == 0 {
                        match affected_base {
                            Some(base) => println!(
                                "nothing to run: task `{task}` has no work affected by changes since `{base}`"
                            ),
                            None => {
                                println!("nothing to run: task `{task}` produced an empty plan")
                            }
                        }
                    }

                    let mut started = 0usize;
                    for resolved in root_plan {
                        started += 1;
                        println!(
                            "{}",
                            render_run_start(
                                &resolved.render(),
                                started,
                                started,
                                total_tasks,
                                stdout_is_terminal,
                            )
                        );
                        execute_task(&resolved.command, &root, &std::collections::BTreeMap::new())?;
                    }
                    for unit in batch_execution_plan(plan, &tasks, &root)? {
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
                Command::Version { .. } | Command::Stamp { .. } => unreachable!(),
            }
        }
    }

    Ok(())
}

struct AffectedScope {
    packages: std::collections::BTreeSet<manifest::PackageId>,
    has_changes: bool,
}

fn calculate_affected_scope(
    root: &std::path::Path,
    graph: &WorkspaceGraph,
    base: Option<&str>,
) -> Result<Option<AffectedScope>> {
    base.map(|base| {
        let changed = git::get_changed_files(root, base)?;
        let has_changes = !changed.is_empty();
        let packages = graph.affected_packages(root, &changed)?;
        Ok(AffectedScope {
            packages,
            has_changes,
        })
    })
    .transpose()
}

fn task_plan_with_scope(
    graph: &WorkspaceGraph,
    tasks: &TaskRegistry,
    task: &str,
    affected: Option<&std::collections::BTreeSet<manifest::PackageId>>,
) -> Result<Vec<tasks::ResolvedTask>> {
    match affected {
        Some(affected) => graph.task_plan_affected(tasks, task, Some(affected)),
        None => graph.task_plan(tasks, task),
    }
}

fn render_task_plan_with_scope(
    graph: &WorkspaceGraph,
    tasks: &TaskRegistry,
    task: &str,
    affected: Option<&std::collections::BTreeSet<manifest::PackageId>>,
) -> Result<String> {
    match affected {
        Some(affected) => graph.render_task_plan_tree_affected(tasks, task, Some(affected)),
        None => graph.render_task_plan_tree(tasks, task),
    }
}

fn task_execution_plan_with_scope(
    graph: &WorkspaceGraph,
    tasks: &TaskRegistry,
    task: &str,
    affected: Option<&std::collections::BTreeSet<manifest::PackageId>>,
) -> Result<graph::TaskExecutionPlan> {
    match affected {
        Some(affected) => graph.task_execution_plan_affected(tasks, task, Some(affected)),
        None => graph.task_execution_plan(tasks, task),
    }
}

/// Compute the next release version for the current branch, or `None` when the
/// commits since the last production tag warrant no release (only non-releasing
/// Conventional Commit types — `docs`, `chore`, etc.).
fn calculate_version(
    root: &std::path::Path,
    channel_override: Option<String>,
) -> Result<Option<String>> {
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

    let Some(next) = version::calculate_next_version(current, &commits) else {
        return Ok(None);
    };
    let base = next.format();

    let full_version = if channel_config.prerelease {
        let count = git::get_existing_prerelease_count(&base, &channel_config.channel) + 1;
        format!("{}-{}.{}", base, channel_config.channel, count)
    } else {
        base
    };

    Ok(Some(full_version))
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
        ExecutionUnit::Single(resolved) => execute_task(
            &resolved.command,
            &resolved.package_dir,
            &resolved.variables,
        ),
        ExecutionUnit::Batch(batch) => {
            let status = match &batch.command {
                TaskCommand::Shell(command) => std::process::Command::new("sh")
                    .arg("-lc")
                    .arg(command)
                    .current_dir(&batch.working_dir)
                    .envs(&batch.variables)
                    .status()?,
                TaskCommand::Argv(command) => {
                    let (program, args) = command
                        .split_first()
                        .ok_or_else(|| anyhow::anyhow!("batched task command was empty"))?;
                    std::process::Command::new(program)
                        .args(args)
                        .current_dir(&batch.working_dir)
                        .envs(&batch.variables)
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
    use super::{execute_task, handle_task_failure, normalize_args};
    use crate::cli::{Cli, Command};
    use crate::graph::{TaskExecutionPlan, WorkspaceGraph};
    use crate::manifest::{Ecosystem, Package, PackageId, TaskOptIn};
    use crate::plugins::{ExecutionUnit, batch_execution_plan, batch_execution_units};
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
            Command::Plan {
                task,
                affected,
                ordered,
                stamp_args,
            } => {
                assert_eq!(task, "build");
                assert_eq!(affected, None);
                assert!(ordered);
                assert!(!stamp_args);
            }
            other => panic!("expected plan command, got {other:?}"),
        }
    }

    #[test]
    fn parses_stamp_args_plan_flag() {
        let cli = Cli::parse_from(["cargo-flux", "plan", "publish", "--stamp-args"]);
        match cli.command {
            Command::Plan {
                task,
                affected,
                ordered,
                stamp_args,
            } => {
                assert_eq!(task, "publish");
                assert_eq!(affected, None);
                assert!(!ordered);
                assert!(stamp_args);
            }
            other => panic!("expected plan command, got {other:?}"),
        }
    }

    #[test]
    fn parses_affected_filter_for_plan_and_run() {
        let plan = Cli::parse_from(["cargo-flux", "plan", "check", "--affected", "origin/main"]);
        match plan.command {
            Command::Plan { affected, .. } => {
                assert_eq!(affected.as_deref(), Some("origin/main"));
            }
            other => panic!("expected plan command, got {other:?}"),
        }

        let run = Cli::parse_from(["cargo-flux", "run", "check", "--affected-from", "main"]);
        match run.command {
            Command::Run { affected, .. } => assert_eq!(affected.as_deref(), Some("main")),
            other => panic!("expected run command, got {other:?}"),
        }

        let affected = Cli::parse_from(["cargo-flux", "affected", "--base", "origin/dev"]);
        match affected.command {
            Command::Affected { base } => assert_eq!(base, "origin/dev"),
            other => panic!("expected affected command, got {other:?}"),
        }
    }

    #[test]
    fn executes_root_task_once_from_workspace_root() {
        let root = temp_dir("execute-root-task");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.prepare]
root = ["sh", "-c", "pwd > root-task-pwd"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = registry.root_task_plan("prepare").expect("root plan");

        assert_eq!(plan.len(), 1);
        execute_task(&plan[0].command, &root, &BTreeMap::new()).expect("execute root task");

        let actual = fs::read_to_string(root.join("root-task-pwd")).expect("read task output");
        assert_eq!(PathBuf::from(actual.trim()), root);
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

    #[test]
    fn batches_compatible_cargo_tasks_across_dependency_layers() {
        let root = temp_dir("cross-layer-cargo-batch");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.check]
autoapply = "inherit"
cascade = "all"
workspace_batchable = true
cargo = ["cargo", "check"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let base = cargo_package("base");
        let mut middle = cargo_package("middle");
        middle.internal_dependencies.push(base.id.clone());
        let mut app = cargo_package("app");
        app.internal_dependencies.push(middle.id.clone());
        app.task_opt_ins
            .insert("check".to_string(), TaskOptIn::default());
        let graph = WorkspaceGraph::new(vec![base, middle, app]);
        let plan = graph
            .task_execution_plan(&registry, "check")
            .expect("materialize plan");

        let units = batch_execution_plan(plan, &registry, &root).expect("schedule plan");
        assert_eq!(units.len(), 1);
        let ExecutionUnit::Batch(batch) = &units[0] else {
            panic!("expected one cross-layer Cargo batch");
        };
        assert_eq!(batch.package_names, vec!["app", "base", "middle"]);
    }

    #[test]
    fn non_cargo_task_remains_a_cross_layer_batching_barrier() {
        let root = temp_dir("cross-layer-cargo-barrier");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.check]
workspace_batchable = true
cargo = ["cargo", "check"]
bun = ["bun", "test"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let mut bun = cargo_resolved_task("sdk", true);
        bun.ecosystem = Ecosystem::Js;
        bun.command = TaskCommand::Argv(vec!["bun".to_string(), "test".to_string()]);
        let plan = TaskExecutionPlan {
            tasks: vec![
                cargo_resolved_task("core", true),
                bun,
                cargo_resolved_task("app", true),
            ],
            prerequisites: vec![vec![], vec![0], vec![1]],
            absorbable_prerequisites: vec![Default::default(); 3],
        };

        let units = batch_execution_plan(plan, &registry, &root).expect("schedule plan");
        assert_eq!(units.len(), 3);
        assert!(matches!(&units[0], ExecutionUnit::Single(task) if task.package_name == "core"));
        assert!(matches!(&units[1], ExecutionUnit::Single(task) if task.package_name == "sdk"));
        assert!(matches!(&units[2], ExecutionUnit::Single(task) if task.package_name == "app"));
    }

    #[test]
    fn cargo_package_selectors_precede_test_harness_arguments() {
        let root = temp_dir("cargo-batch-harness-args");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.check]
workspace_batchable = true
cargo = ["cargo", "test", "--", "--nocapture"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let mut a = cargo_resolved_task("a", true);
        let mut b = cargo_resolved_task("b", true);
        for task in [&mut a, &mut b] {
            task.command = TaskCommand::Argv(vec![
                "cargo".to_string(),
                "test".to_string(),
                "--".to_string(),
                "--nocapture".to_string(),
            ]);
        }

        let units = batch_execution_units(vec![a, b], &registry, &root).expect("batch units");
        let ExecutionUnit::Batch(batch) = &units[0] else {
            panic!("expected Cargo batch");
        };
        assert_eq!(
            batch.command,
            TaskCommand::Argv(vec![
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                "a".to_string(),
                "-p".to_string(),
                "b".to_string(),
                "--".to_string(),
                "--nocapture".to_string(),
            ])
        );
    }

    #[test]
    fn cargo_batches_require_matching_variable_environments() {
        let root = temp_dir("cargo-batch-variables");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.check]
workspace_batchable = true
variables = ["PROFILE"]
cargo = ["cargo", "check"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let mut a = cargo_resolved_task("a", true);
        let mut b = cargo_resolved_task("b", true);
        a.variables
            .insert("PROFILE".to_string(), "fast".to_string());
        b.variables
            .insert("PROFILE".to_string(), "slow".to_string());

        let units = batch_execution_units(vec![a, b], &registry, &root).expect("batch units");
        assert_eq!(units.len(), 2);
        assert!(
            units
                .iter()
                .all(|unit| matches!(unit, ExecutionUnit::Single(_)))
        );
    }

    #[test]
    fn cargo_batch_retains_matching_variable_environment() {
        let root = temp_dir("cargo-batch-matching-variables");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.check]
workspace_batchable = true
variables = ["PROFILE"]
cargo = ["cargo", "check"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let mut a = cargo_resolved_task("a", true);
        let mut b = cargo_resolved_task("b", true);
        for task in [&mut a, &mut b] {
            task.variables
                .insert("PROFILE".to_string(), "fast".to_string());
        }

        let units = batch_execution_units(vec![a, b], &registry, &root).expect("batch units");
        let ExecutionUnit::Batch(batch) = &units[0] else {
            panic!("expected Cargo batch");
        };
        assert_eq!(
            batch.variables.get("PROFILE").map(String::as_str),
            Some("fast")
        );
    }

    fn cargo_package(package_name: &str) -> Package {
        Package {
            id: PackageId::new(Ecosystem::Cargo, package_name),
            name: package_name.to_string(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: PathBuf::from(format!("{package_name}/Cargo.toml")),
            js_package_manager: None,
            task_opt_ins: Default::default(),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![],
        }
    }

    fn cargo_resolved_task(package_name: &str, explicitly_opted_in: bool) -> ResolvedTask {
        let package = cargo_package(package_name);

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
            Command::Stamp {
                version,
                packages,
                exclude,
                exclude_versions,
            } => {
                assert_eq!(version.as_deref(), Some("1.2.3"));
                assert!(packages.is_empty());
                assert!(exclude.is_empty());
                assert!(exclude_versions.is_empty());
            }
            other => panic!("expected stamp command, got {other:?}"),
        }
    }

    #[test]
    fn parses_repeated_stamp_selectors() {
        let cli = Cli::parse_from([
            "cargo-flux",
            "stamp",
            "1.2.3",
            "-p",
            "sdk",
            "--package",
            "app",
            "--exclude",
            "private",
            "--exclude-version",
            "0.0.0",
            "--exclude-version",
            "0.0.1",
        ]);
        match cli.command {
            Command::Stamp {
                packages,
                exclude,
                exclude_versions,
                ..
            } => {
                assert_eq!(packages, ["sdk", "app"]);
                assert_eq!(exclude, ["private"]);
                assert_eq!(exclude_versions, ["0.0.0", "0.0.1"]);
            }
            other => panic!("expected stamp command, got {other:?}"),
        }
    }

    #[test]
    fn parses_stamp_command_without_version() {
        let cli = Cli::parse_from(["cargo-flux", "stamp"]);
        match cli.command {
            Command::Stamp { version, .. } => {
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
