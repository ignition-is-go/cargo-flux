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
use cli::{Cli, Command, ReportCommand};
use graph::WorkspaceGraph;
use manifest::discover_workspace;
use plugins::{ExecutionUnit, batch_execution_plan};
use serde::{Deserialize, Serialize};
use std::io::{IsTerminal, Write};
use tasks::{ResolvedRootTask, TaskCommand, TaskRegistry};

fn main() -> Result<()> {
    let cli = Cli::parse_from(normalize_args(std::env::args_os()));
    if let Command::Report { command } = &cli.command {
        match command {
            ReportCommand::GithubOutput { input, output_file } => {
                append_github_outputs(input, output_file)?;
            }
        }
        return Ok(());
    }
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
                Command::Run {
                    task,
                    affected,
                    report,
                } => {
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
                    let root_execution = execute_root_plan(
                        &root_plan,
                        &root,
                        total_tasks,
                        stdout_is_terminal,
                        &mut started,
                    )?;
                    let root_outcomes = root_execution.outcomes;
                    let task_outputs = root_execution.task_outputs;

                    let target_skipped = has_skipped_root_dependency(&root_outcomes);
                    if !target_skipped {
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
                    } else if !plan.tasks.is_empty() {
                        let count = plan.tasks.len();
                        started += count;
                        println!(
                            "{} — skipped by root condition",
                            render_run_start(
                                "package tasks",
                                started + 1 - count,
                                started,
                                total_tasks,
                                stdout_is_terminal,
                            )
                        );
                    }

                    let execution_report = ExecutionReport {
                        schema_version: execution_report_schema_version(),
                        outcome: if total_tasks == 0 || target_skipped {
                            TaskExecutionOutcome::Skipped
                        } else {
                            TaskExecutionOutcome::Completed
                        },
                        task_outputs,
                    };
                    if let Some(path) = report {
                        write_execution_report(&path, &execution_report)?;
                    }
                }
                Command::Version { .. } | Command::Stamp { .. } | Command::Report { .. } => {
                    unreachable!()
                }
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

fn has_skipped_root_dependency(
    outcomes: &std::collections::BTreeMap<String, TaskExecutionOutcome>,
) -> bool {
    outcomes
        .values()
        .any(|outcome| matches!(outcome, TaskExecutionOutcome::Skipped))
}

struct RootExecution {
    outcomes: std::collections::BTreeMap<String, TaskExecutionOutcome>,
    task_outputs: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

fn execute_root_plan(
    plan: &[ResolvedRootTask],
    root: &std::path::Path,
    total_tasks: usize,
    use_color: bool,
    started: &mut usize,
) -> Result<RootExecution> {
    let mut task_outputs = std::collections::BTreeMap::new();
    let mut outcomes = std::collections::BTreeMap::new();
    for resolved in plan {
        *started += 1;
        let dependency_skipped = resolved.depends_on.iter().any(|dependency| {
            matches!(
                outcomes.get(dependency),
                Some(TaskExecutionOutcome::Skipped)
            )
        });
        let condition_met = resolved.condition.as_ref().is_none_or(|condition| {
            task_outputs
                .get(&condition.task_name)
                .and_then(|outputs: &std::collections::BTreeMap<String, String>| {
                    outputs.get(&condition.output_name)
                })
                .is_some_and(|value| !value.is_empty())
        });
        if dependency_skipped || !condition_met {
            println!(
                "{} — skipped",
                render_run_start(
                    &resolved.render(),
                    *started,
                    *started,
                    total_tasks,
                    use_color,
                )
            );
            outcomes.insert(resolved.task_name.clone(), TaskExecutionOutcome::Skipped);
            continue;
        }

        println!(
            "{}",
            render_run_start(
                &resolved.render(),
                *started,
                *started,
                total_tasks,
                use_color,
            )
        );
        let mut captured_stdout = None;
        for (index, command) in resolved.commands.iter().enumerate() {
            let command = command.interpolate_outputs(&task_outputs)?;
            let capture = !resolved.output_names.is_empty() && index + 1 == resolved.commands.len();
            let output = execute_task_with_output(
                &command,
                root,
                &std::collections::BTreeMap::new(),
                capture,
            )?;
            if output.is_some() {
                captured_stdout = output;
            }
        }
        if !resolved.output_names.is_empty() {
            let value = captured_stdout
                .unwrap_or_default()
                .trim_end_matches(['\r', '\n'])
                .to_string();
            task_outputs.insert(
                resolved.task_name.clone(),
                resolved
                    .output_names
                    .iter()
                    .map(|name| (name.clone(), value.clone()))
                    .collect(),
            );
        }
        outcomes.insert(resolved.task_name.clone(), TaskExecutionOutcome::Completed);
    }
    Ok(RootExecution {
        outcomes,
        task_outputs,
    })
}

fn execute_task(
    command: &TaskCommand,
    cwd: &std::path::Path,
    variables: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    execute_task_with_output(command, cwd, variables, false).map(|_| ())
}

fn execute_task_with_output(
    command: &TaskCommand,
    cwd: &std::path::Path,
    variables: &std::collections::BTreeMap<String, String>,
    capture_stdout: bool,
) -> Result<Option<String>> {
    let mut cmd = match command {
        TaskCommand::Shell(command) => {
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-lc").arg(command);
            cmd
        }
        TaskCommand::Argv(command) => {
            let (program, args) = command
                .split_first()
                .ok_or_else(|| anyhow::anyhow!("resolved task command was empty"))?;
            let mut cmd = std::process::Command::new(program);
            cmd.args(args);
            cmd
        }
    };
    cmd.current_dir(cwd).envs(variables);
    if capture_stdout {
        cmd.stderr(std::process::Stdio::inherit());
        let output = cmd.output()?;
        std::io::stdout().write_all(&output.stdout)?;
        if !output.stdout.is_empty() && !output.stdout.ends_with(b"\n") {
            writeln!(std::io::stdout())?;
        }
        anyhow::ensure!(
            output.status.success(),
            "task execution failed in {}",
            cwd.display()
        );
        return String::from_utf8(output.stdout)
            .map(Some)
            .map_err(|error| anyhow::anyhow!("task stdout was not UTF-8: {error}"));
    }

    let status = cmd.status()?;
    anyhow::ensure!(
        status.success(),
        "task execution failed in {}",
        cwd.display()
    );
    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum TaskExecutionOutcome {
    Completed,
    Skipped,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExecutionReport {
    schema_version: u32,
    outcome: TaskExecutionOutcome,
    task_outputs: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

const fn execution_report_schema_version() -> u32 {
    1
}

fn write_execution_report(path: &std::path::Path, report: &ExecutionReport) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let serialized = serde_json::to_vec(report)?;
    let mut temp_path = None;
    for attempt in 0..100u32 {
        let candidate = parent.join(format!(
            ".cargo-flux-report-{}-{}-{attempt}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(&serialized)?;
                file.sync_all()?;
                temp_path = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let temp = temp_path.ok_or_else(|| anyhow::anyhow!("could not create report temp file"))?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

fn append_github_outputs(input: &std::path::Path, output_file: &std::path::Path) -> Result<()> {
    let report: ExecutionReport = serde_json::from_slice(&std::fs::read(input)?)?;
    anyhow::ensure!(
        report.schema_version == execution_report_schema_version(),
        "unsupported execution report schema version {}",
        report.schema_version
    );
    let outcome = match report.outcome {
        TaskExecutionOutcome::Completed => "completed",
        TaskExecutionOutcome::Skipped => "skipped",
    };
    let task_outputs = serde_json::to_string(&report.task_outputs)?;
    let payload = format!("outcome={outcome}\ntask-outputs={task_outputs}\n");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(output_file)?;
    file.write_all(payload.as_bytes())?;
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
    use super::{
        ExecutionReport, TaskExecutionOutcome, append_github_outputs, execute_root_plan,
        execute_task, execution_report_schema_version, handle_task_failure,
        has_skipped_root_dependency, normalize_args, write_execution_report,
    };
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
        execute_task(&plan[0].commands[0], &root, &BTreeMap::new()).expect("execute root task");

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
    fn github_adapter_rejects_unversioned_report_without_writing() {
        let root = temp_dir("unversioned-report");
        let report = root.join("report.json");
        let output = root.join("github-output");
        fs::write(&report, r#"{"outcome":"completed","task_outputs":{}}"#).expect("write report");
        fs::write(&output, "existing=value\n").expect("write output");

        assert!(append_github_outputs(&report, &output).is_err());
        assert_eq!(
            fs::read_to_string(output).expect("read output"),
            "existing=value\n"
        );
    }

    #[test]
    fn parses_run_report_path() {
        let cli = Cli::parse_from(["cargo-flux", "run", "ship", "--report", "report.json"]);
        match cli.command {
            Command::Run { task, report, .. } => {
                assert_eq!(task, "ship");
                assert_eq!(report, Some(PathBuf::from("report.json")));
            }
            other => panic!("expected run command, got {other:?}"),
        }
    }

    #[test]
    fn parses_report_github_output_adapter() {
        let cli = Cli::parse_from([
            "cargo-flux",
            "report",
            "github-output",
            "--input",
            "report.json",
            "--output-file",
            "github-output",
        ]);
        match cli.command {
            Command::Report {
                command: crate::cli::ReportCommand::GithubOutput { input, output_file },
            } => {
                assert_eq!(input, PathBuf::from("report.json"));
                assert_eq!(output_file, PathBuf::from("github-output"));
            }
            other => panic!("expected report adapter, got {other:?}"),
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

    #[test]
    fn empty_root_output_skips_dependent_chain() {
        let root = temp_dir("empty-output-skip");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.calculate-version]
root = ["printf", ""]
outputs = { version = "stdout" }

[tasks.publish]
depends_on = ["calculate-version"]
when = { output = "calculate-version.version", nonempty = true }
root = ["touch", "published"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = registry.root_task_plan("publish").expect("root plan");
        let mut started = 0;
        let execution = execute_root_plan(&plan, &root, plan.len(), false, &mut started)
            .expect("execute root plan");

        assert_eq!(
            execution.outcomes.get("publish"),
            Some(&TaskExecutionOutcome::Skipped)
        );
        assert_eq!(execution.task_outputs["calculate-version"]["version"], "");
        assert!(!root.join("published").exists());
    }

    #[test]
    fn skipped_root_dependency_blocks_package_only_target() {
        let root = temp_dir("skipped-root-blocks-packages");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.probe]
root = ["printf", ""]
outputs = { value = "stdout" }

[tasks.gate]
depends_on = ["probe"]
when = { output = "probe.value", nonempty = true }
root = ["touch", "gate-ran"]

[tasks.package-check]
depends_on = ["gate"]
autoapply = "all"
cargo = ["touch", "package-ran"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = registry.root_task_plan("package-check").expect("root plan");
        let mut started = 0;
        let execution =
            execute_root_plan(&plan, &root, 3, false, &mut started).expect("execute roots");

        assert!(has_skipped_root_dependency(&execution.outcomes));
        assert!(!root.join("gate-ran").exists());
        assert!(!root.join("package-ran").exists());
    }

    #[test]
    fn root_output_substitution_stays_one_argv_value() {
        let root = temp_dir("output-argv-substitution");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.calculate-version]
root = ["printf", "v 1;not-shell\n"]
outputs = { version = "stdout" }

[tasks.publish]
depends_on = ["calculate-version"]
when = { output = "calculate-version.version", nonempty = true }
root_steps = [
  ["touch", "${calculate-version.version}"],
  ["printf", "published\n"],
]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = registry.root_task_plan("publish").expect("root plan");
        let mut started = 0;
        let execution = execute_root_plan(&plan, &root, plan.len(), false, &mut started)
            .expect("execute root plan");

        assert_eq!(
            execution.outcomes.get("publish"),
            Some(&TaskExecutionOutcome::Completed)
        );
        assert!(root.join("v 1;not-shell").exists());
        assert!(!root.join("not-shell").exists());
    }

    #[test]
    fn execution_report_adapts_to_safe_github_outputs() {
        let root = temp_dir("github-report-output");
        let report_path = root.join("report.json");
        let github_path = root.join("github-output");
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "probe".to_string(),
            BTreeMap::from([
                ("empty".to_string(), String::new()),
                ("hostile".to_string(), "line1\nline2=%<<\\\"☃".to_string()),
            ]),
        );
        let report = ExecutionReport {
            schema_version: execution_report_schema_version(),
            outcome: TaskExecutionOutcome::Completed,
            task_outputs: outputs.clone(),
        };
        write_execution_report(&report_path, &report).expect("write report");
        fs::write(&github_path, "existing=value\n").expect("seed output");
        append_github_outputs(&report_path, &github_path).expect("adapt report");

        let actual = fs::read_to_string(github_path).expect("read output");
        let lines = actual.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "existing=value");
        assert_eq!(lines[1], "outcome=completed");
        let json = lines[2]
            .strip_prefix("task-outputs=")
            .expect("JSON output prefix");
        let decoded: BTreeMap<String, BTreeMap<String, String>> =
            serde_json::from_str(json).expect("decode JSON");
        assert_eq!(decoded, outputs);
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
