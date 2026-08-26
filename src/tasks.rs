use crate::manifest::{Ecosystem, JsPackageManager, Package, colorize_display_label};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub struct TaskRegistry {
    tasks: BTreeMap<String, TaskDefinition>,
    channels: Option<toml::Value>,
}

impl TaskRegistry {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("flux.toml");
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read task config {}", path.display()))?;
        let config = toml::from_str::<FluxConfig>(&content)
            .with_context(|| format!("failed to parse task config {}", path.display()))?;
        Ok(Self {
            tasks: config.tasks.unwrap_or_default(),
            channels: config.channels,
        })
    }

    pub fn channels(&self) -> Option<&toml::Value> {
        self.channels.as_ref()
    }

    /// Resolve workspace-root task variants reachable from a logical task.
    /// Each root variant appears once, after its root dependencies.
    pub fn root_task_plan(&self, task_name: &str) -> Result<Vec<ResolvedRootTask>> {
        let mut visiting = BTreeSet::new();
        let mut completed = BTreeSet::new();
        let mut plan = Vec::new();
        self.collect_root_tasks(task_name, &mut visiting, &mut completed, &mut plan)?;
        Ok(plan)
    }

    fn collect_root_tasks(
        &self,
        task_name: &str,
        visiting: &mut BTreeSet<String>,
        completed: &mut BTreeSet<String>,
        plan: &mut Vec<ResolvedRootTask>,
    ) -> Result<()> {
        if completed.contains(task_name) {
            return Ok(());
        }
        if !visiting.insert(task_name.to_string()) {
            bail!("cycle detected in root task dependencies involving `{task_name}`");
        }

        let task = self
            .tasks
            .get(task_name)
            .ok_or_else(|| anyhow!("task `{task_name}` is not defined in flux.toml"))?;
        let has_root = task.has_root_command()?;
        for dependency in &task.depends_on {
            let dependency_task = self.tasks.get(dependency).ok_or_else(|| {
                anyhow!("task `{task_name}` depends on undefined task `{dependency}` in flux.toml")
            })?;
            if has_root && !dependency_task.has_root_command()? {
                bail!(
                    "root task `{task_name}` depends on `{dependency}`, but `{dependency}` has no root command"
                );
            }
            self.collect_root_tasks(dependency, visiting, completed, plan)?;
        }

        let condition = task
            .when
            .as_ref()
            .map(|condition| self.resolve_output_condition(task_name, condition))
            .transpose()?;
        if task.outputs.len() > 1 {
            bail!(
                "task `{task_name}` declares multiple outputs, but the `stdout` source currently supports one"
            );
        }
        if !task.outputs.is_empty() && !is_output_task_name(task_name) {
            bail!(
                "output-producing task name `{task_name}` must start with a letter or `_` and contain only letters, digits, `_`, `-`, or `.`"
            );
        }
        for output_name in task.outputs.keys() {
            if !is_output_identifier(output_name) {
                bail!(
                    "task `{task_name}` output name `{output_name}` must start with a letter or `_` and contain only letters, digits, `_`, or `-`"
                );
            }
        }
        if (!task.outputs.is_empty() || condition.is_some()) && !has_root {
            bail!("task `{task_name}` uses root outputs or conditions but has no root command");
        }
        if (!task.outputs.is_empty() || condition.is_some()) && task.has_package_command() {
            bail!(
                "task `{task_name}` uses dynamic root outputs or conditions and cannot also define package commands"
            );
        }

        visiting.remove(task_name);
        completed.insert(task_name.to_string());
        let commands = task.root_commands()?;
        if !commands.is_empty() {
            if commands.iter().any(TaskCommand::is_empty) {
                bail!("root task `{task_name}` resolved to an empty command");
            }
            for command in &commands {
                self.validate_root_output_references(task_name, command)?;
            }
            plan.push(ResolvedRootTask {
                task_name: task_name.to_string(),
                commands,
                depends_on: task.depends_on.clone(),
                output_names: task.outputs.keys().cloned().collect(),
                condition,
            });
        }
        Ok(())
    }

    fn validate_root_output_references(
        &self,
        task_name: &str,
        command: &TaskCommand,
    ) -> Result<()> {
        match command {
            TaskCommand::Shell(command) => {
                if !qualified_output_references(command, false)?.is_empty() {
                    bail!("task output substitution is supported only in argv commands");
                }
            }
            TaskCommand::Argv(command) => {
                for part in command {
                    for reference in qualified_output_references(part, true)? {
                        self.validate_output_reference(task_name, reference)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_output_reference(&self, task_name: &str, reference: &str) -> Result<()> {
        let (producer, output) = reference.rsplit_once('.').ok_or_else(|| {
            anyhow!("task output reference must be `task.output`, got `{reference}`")
        })?;
        if !self.has_transitive_dependency(task_name, producer, &mut BTreeSet::new())? {
            bail!(
                "task `{task_name}` references `{reference}`, but `{producer}` is not a dependency"
            );
        }
        let producer_task = self
            .tasks
            .get(producer)
            .ok_or_else(|| anyhow!("task `{task_name}` references undefined task `{producer}`"))?;
        if !producer_task.outputs.contains_key(output) {
            bail!("task `{task_name}` references undefined output `{reference}`");
        }
        Ok(())
    }

    fn resolve_output_condition(
        &self,
        task_name: &str,
        condition: &TaskCondition,
    ) -> Result<OutputCondition> {
        if !condition.nonempty {
            bail!("task `{task_name}` has a `when` condition without `nonempty = true`");
        }
        let (producer, output) = condition.output.rsplit_once('.').ok_or_else(|| {
            anyhow!(
                "task `{task_name}` condition must reference an output as `task.output`, got `{}`",
                condition.output
            )
        })?;
        self.validate_output_reference(task_name, &condition.output)?;
        Ok(OutputCondition {
            task_name: producer.to_string(),
            output_name: output.to_string(),
        })
    }

    fn has_transitive_dependency(
        &self,
        task_name: &str,
        target: &str,
        visited: &mut BTreeSet<String>,
    ) -> Result<bool> {
        if !visited.insert(task_name.to_string()) {
            return Ok(false);
        }
        let task = self
            .tasks
            .get(task_name)
            .ok_or_else(|| anyhow!("task `{task_name}` is not defined in flux.toml"))?;
        for dependency in &task.depends_on {
            if dependency == target
                || self.has_transitive_dependency(dependency, target, visited)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn resolve(&self, package: &Package, task_name: &str) -> Result<ResolvedTask> {
        let task = self
            .tasks
            .get(task_name)
            .ok_or_else(|| anyhow!("task `{task_name}` is not defined in flux.toml"))?;
        let command = task.command_for(package).ok_or_else(|| {
            anyhow!(
                "task `{task_name}` has no variant for {}",
                package.ecosystem
            )
        })?;

        if command.is_empty() {
            bail!(
                "task `{task_name}` for {} resolved to an empty command",
                package.ecosystem
            );
        }
        validate_task_variables(task, package, task_name)?;
        let command = interpolate_command(command, package, task_name)?;

        Ok(ResolvedTask {
            task_name: task_name.to_string(),
            package_name: package.name.clone(),
            ecosystem: package.ecosystem,
            display_label: package.display_label(),
            explicitly_opted_in: package.opts_into_task(task_name),
            package_dir: package
                .manifest_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
            variables: package
                .task_variables(task_name)
                .cloned()
                .unwrap_or_default(),
            command,
        })
    }

    pub fn cascades_to_native_dependencies(&self, task_name: &str) -> Result<bool> {
        self.tasks
            .get(task_name)
            .map(|task| matches!(task.cascade, Cascade::All))
            .ok_or_else(|| anyhow!("task `{task_name}` is not defined in flux.toml"))
    }

    pub fn cascades_across_packages(&self, task_name: &str) -> Result<bool> {
        self.tasks
            .get(task_name)
            .map(|task| !matches!(task.cascade, Cascade::None))
            .ok_or_else(|| anyhow!("task `{task_name}` is not defined in flux.toml"))
    }

    pub fn task_dependencies(&self, task_name: &str) -> Result<Vec<String>> {
        let task = self
            .tasks
            .get(task_name)
            .ok_or_else(|| anyhow!("task `{task_name}` is not defined in flux.toml"))?;
        Ok(task.depends_on.clone())
    }

    pub fn workspace_batchable(&self, task_name: &str) -> Result<bool> {
        let task = self
            .tasks
            .get(task_name)
            .ok_or_else(|| anyhow!("task `{task_name}` is not defined in flux.toml"))?;
        Ok(task.workspace_batchable)
    }

    pub fn autoapply_mode(&self, task_name: &str) -> Result<AutoApply> {
        let task = self
            .tasks
            .get(task_name)
            .ok_or_else(|| anyhow!("task `{task_name}` is not defined in flux.toml"))?;
        Ok(task.autoapply)
    }

    pub fn ecosystem_task_dependencies(
        &self,
        task_name: &str,
        source_ecosystem: Ecosystem,
    ) -> Result<Vec<TargetedTaskDependency>> {
        let task = self
            .tasks
            .get(task_name)
            .ok_or_else(|| anyhow!("task `{task_name}` is not defined in flux.toml"))?;
        Ok(task.ecosystem_depends_on.dependencies_for(source_ecosystem))
    }

    pub fn has_targeted_ecosystem_dependency(
        &self,
        task_name: &str,
        source_ecosystem: Ecosystem,
        target_ecosystem: Ecosystem,
    ) -> Result<bool> {
        Ok(self
            .ecosystem_task_dependencies(task_name, source_ecosystem)?
            .into_iter()
            .any(|dependency| dependency.ecosystem == target_ecosystem))
    }

    pub fn can_inherit_without_explicit_opt_in(&self, package: &Package, task_name: &str) -> bool {
        matches!(
            self.autoapply_mode(task_name).unwrap_or_default(),
            AutoApply::Inherit | AutoApply::All
        ) && self.task_command_exists(package, task_name)
    }

    pub fn seeds_all_packages_without_explicit_opt_in(
        &self,
        package: &Package,
        task_name: &str,
    ) -> bool {
        matches!(
            self.autoapply_mode(task_name).unwrap_or_default(),
            AutoApply::All
        ) && self.task_command_exists(package, task_name)
    }

    pub fn participates_in_task(&self, package: &Package, task_name: &str) -> bool {
        package.opts_into_task(task_name)
            || self.can_inherit_without_explicit_opt_in(package, task_name)
    }

    fn task_command_exists(&self, package: &Package, task_name: &str) -> bool {
        self.tasks
            .get(task_name)
            .and_then(|task| task.command_for(package))
            .is_some()
    }
}

fn interpolate_command(
    command: TaskCommand,
    package: &Package,
    task_name: &str,
) -> Result<TaskCommand> {
    let variables = package
        .task_variables(task_name)
        .cloned()
        .unwrap_or_default();
    match command {
        TaskCommand::Shell(command) => {
            Ok(TaskCommand::Shell(interpolate_part(&command, &variables)?))
        }
        TaskCommand::Argv(command) => Ok(TaskCommand::Argv(
            command
                .into_iter()
                .map(|part| interpolate_part(&part, &variables))
                .collect::<Result<Vec<_>>>()?,
        )),
    }
}

fn validate_task_variables(
    task: &TaskDefinition,
    package: &Package,
    task_name: &str,
) -> Result<()> {
    let variables = package
        .task_variables(task_name)
        .cloned()
        .unwrap_or_default();
    for name in &task.variables {
        if !variables.contains_key(name) {
            bail!(
                "package `{}` opted into `{}` but did not provide required task variable `{}`",
                package.name,
                task_name,
                name
            );
        }
    }
    Ok(())
}

fn interpolate_part(part: &str, variables: &BTreeMap<String, String>) -> Result<String> {
    let mut result = String::new();
    let mut rest = part;

    while let Some(start) = rest.find("${") {
        result.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            bail!("unclosed task variable reference in `{part}`");
        };
        let name = &after_start[..end];
        let value = variables
            .get(name)
            .ok_or_else(|| anyhow!("task variable `{name}` was not provided"))?;
        result.push_str(value);
        rest = &after_start[end + 1..];
    }

    result.push_str(rest);
    Ok(result)
}

#[derive(Debug, Clone)]
pub struct ResolvedTask {
    pub task_name: String,
    pub package_name: String,
    pub ecosystem: Ecosystem,
    pub display_label: String,
    pub explicitly_opted_in: bool,
    pub package_dir: std::path::PathBuf,
    pub variables: BTreeMap<String, String>,
    pub command: TaskCommand,
}

impl ResolvedTask {
    pub fn render(&self) -> String {
        self.render_colored(false)
    }

    pub fn render_colored(&self, use_color: bool) -> String {
        let variables = if self.variables.is_empty() {
            String::new()
        } else {
            format!(
                "{}{{{}}}{}",
                if use_color { "\x1b[2m " } else { " " },
                self.variables
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if use_color { "\x1b[0m" } else { "" },
            )
        };

        let package_name = if use_color {
            format!("\x1b[1m{}\x1b[0m", self.package_name)
        } else {
            self.package_name.clone()
        };
        let task_name = if use_color {
            format!("\x1b[1;97m{}\x1b[0m", self.task_name)
        } else {
            self.task_name.clone()
        };
        let display_label = colorize_display_label(
            &self.display_label,
            self.ecosystem,
            infer_js_package_manager(&self.display_label),
            use_color,
        );

        format!(
            "{}:{} [{}]{}",
            package_name, task_name, display_label, variables
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRootTask {
    pub task_name: String,
    pub commands: Vec<TaskCommand>,
    pub depends_on: Vec<String>,
    pub output_names: Vec<String>,
    pub condition: Option<OutputCondition>,
}

impl ResolvedRootTask {
    pub fn render(&self) -> String {
        format!(
            "workspace:{} [root]{}",
            self.task_name,
            if self.condition.is_some() {
                " (conditional)"
            } else {
                ""
            }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputCondition {
    pub task_name: String,
    pub output_name: String,
}

fn infer_js_package_manager(label: &str) -> Option<JsPackageManager> {
    match label {
        "npm" => Some(JsPackageManager::Npm),
        "pnpm" => Some(JsPackageManager::Pnpm),
        "yarn" => Some(JsPackageManager::Yarn),
        "bun" => Some(JsPackageManager::Bun),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FluxConfig {
    tasks: Option<BTreeMap<String, TaskDefinition>>,
    channels: Option<toml::Value>,
    // Stamp policy is loaded by the stamp command; accepting it here keeps the
    // shared flux.toml schema compatible with task and version commands.
    #[serde(rename = "stamp")]
    _stamp: Option<toml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskDefinition {
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    ecosystem_depends_on: EcosystemTaskDependencies,
    #[serde(default)]
    autoapply: AutoApply,
    #[serde(default)]
    cascade: Cascade,
    #[serde(default)]
    workspace_batchable: bool,
    #[serde(default)]
    variables: Vec<String>,
    /// Command executed once from the workspace root.
    root: Option<CommandSpec>,
    /// Structured commands executed sequentially from the workspace root.
    root_steps: Option<Vec<Vec<String>>>,
    /// Named values captured from the final root command's stdout.
    #[serde(default)]
    outputs: BTreeMap<String, TaskOutputSource>,
    /// Runtime condition evaluated from a dependency's output.
    when: Option<TaskCondition>,
    #[serde(rename = "default")]
    default_command: Option<CommandSpec>,
    cargo: Option<CommandSpec>,
    npm: Option<CommandSpec>,
    pnpm: Option<CommandSpec>,
    yarn: Option<CommandSpec>,
    bun: Option<CommandSpec>,
    uv: Option<CommandSpec>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum TaskOutputSource {
    Stdout,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskCondition {
    output: String,
    #[serde(default)]
    nonempty: bool,
}

#[derive(Debug, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "kebab-case")]
enum Cascade {
    All,
    None,
    #[default]
    BridgeOnly,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct EcosystemTaskDependencies {
    #[serde(default, deserialize_with = "deserialize_targeted_task_dependencies")]
    cargo: Vec<TargetedTaskDependency>,
    #[serde(default, deserialize_with = "deserialize_targeted_task_dependencies")]
    js: Vec<TargetedTaskDependency>,
    #[serde(default, deserialize_with = "deserialize_targeted_task_dependencies")]
    uv: Vec<TargetedTaskDependency>,
}

impl EcosystemTaskDependencies {
    fn dependencies_for(&self, source_ecosystem: Ecosystem) -> Vec<TargetedTaskDependency> {
        match source_ecosystem {
            Ecosystem::Cargo => self.cargo.clone(),
            Ecosystem::Js => self.js.clone(),
            Ecosystem::Uv => self.uv.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetedTaskDependency {
    pub ecosystem: Ecosystem,
    pub task_name: String,
}

fn deserialize_targeted_task_dependencies<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<TargetedTaskDependency>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| {
            let (ecosystem, task_name) = value.split_once(':').ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "expected ecosystem-scoped task dependency like `cargo:gen`, got `{value}`"
                ))
            })?;
            let ecosystem = ecosystem
                .parse::<Ecosystem>()
                .map_err(serde::de::Error::custom)?;
            Ok(TargetedTaskDependency {
                ecosystem,
                task_name: task_name.to_string(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AutoApply {
    #[default]
    None,
    Inherit,
    All,
}

impl<'de> Deserialize<'de> for AutoApply {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "kebab-case")]
        enum AutoApplyMode {
            None,
            Inherit,
            All,
        }

        match AutoApplyMode::deserialize(deserializer)? {
            AutoApplyMode::None => Ok(AutoApply::None),
            AutoApplyMode::Inherit => Ok(AutoApply::Inherit),
            AutoApplyMode::All => Ok(AutoApply::All),
        }
    }
}

impl TaskDefinition {
    fn has_root_command(&self) -> Result<bool> {
        if self.root.is_some() && self.root_steps.is_some() {
            bail!("a task cannot define both `root` and `root_steps`");
        }
        if self.root_steps.as_ref().is_some_and(Vec::is_empty) {
            bail!("a task's `root_steps` cannot be empty");
        }
        Ok(self.root.is_some() || self.root_steps.is_some())
    }

    fn root_commands(&self) -> Result<Vec<TaskCommand>> {
        self.has_root_command()?;
        Ok(match &self.root {
            Some(command) => vec![command.to_command()],
            None => self
                .root_steps
                .as_deref()
                .unwrap_or_default()
                .iter()
                .cloned()
                .map(TaskCommand::Argv)
                .collect(),
        })
    }

    fn has_package_command(&self) -> bool {
        self.default_command.is_some()
            || self.cargo.is_some()
            || self.npm.is_some()
            || self.pnpm.is_some()
            || self.yarn.is_some()
            || self.bun.is_some()
            || self.uv.is_some()
    }

    fn command_for(&self, package: &Package) -> Option<TaskCommand> {
        let spec = match package.ecosystem {
            Ecosystem::Cargo => self.cargo.as_ref().or(self.default_command.as_ref()),
            Ecosystem::Js => match package.js_package_manager.unwrap_or(JsPackageManager::Npm) {
                JsPackageManager::Pnpm => self
                    .pnpm
                    .as_ref()
                    .or(self.npm.as_ref())
                    .or(self.default_command.as_ref()),
                JsPackageManager::Yarn => self
                    .yarn
                    .as_ref()
                    .or(self.npm.as_ref())
                    .or(self.default_command.as_ref()),
                JsPackageManager::Bun => self
                    .bun
                    .as_ref()
                    .or(self.npm.as_ref())
                    .or(self.default_command.as_ref()),
                JsPackageManager::Npm => self.npm.as_ref().or(self.default_command.as_ref()),
            },
            Ecosystem::Uv => self.uv.as_ref().or(self.default_command.as_ref()),
        }?;
        Some(spec.to_command())
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CommandSpec {
    String(String),
    Array(Vec<String>),
}

impl CommandSpec {
    fn to_command(&self) -> TaskCommand {
        match self {
            CommandSpec::String(command) => TaskCommand::Shell(command.clone()),
            CommandSpec::Array(command) => TaskCommand::Argv(command.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskCommand {
    Shell(String),
    Argv(Vec<String>),
}

impl TaskCommand {
    fn is_empty(&self) -> bool {
        match self {
            TaskCommand::Shell(command) => command.trim().is_empty(),
            TaskCommand::Argv(command) => command.first().is_none_or(|program| program.is_empty()),
        }
    }

    pub fn interpolate_outputs(
        &self,
        outputs: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<Self> {
        match self {
            // Shell commands remain an explicit escape hatch. Flux never rewrites
            // their source because doing so would change shell expansion semantics.
            TaskCommand::Shell(command) => {
                if !qualified_output_references(command, false)?.is_empty() {
                    bail!("task output substitution is supported only in argv commands");
                }
                Ok(TaskCommand::Shell(command.clone()))
            }
            TaskCommand::Argv(command) => Ok(TaskCommand::Argv(
                command
                    .iter()
                    .map(|part| interpolate_output_part(part, outputs))
                    .collect::<Result<Vec<_>>>()?,
            )),
        }
    }
}

fn is_output_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && chars.all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

fn is_output_task_name(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && chars.all(|character| {
            character.is_ascii_alphanumeric()
                || character == '_'
                || character == '-'
                || character == '.'
        })
}

fn is_output_reference_syntax(reference: &str) -> bool {
    reference
        .rsplit_once('.')
        .is_some_and(|(task_name, output_name)| {
            is_output_task_name(task_name) && is_output_identifier(output_name)
        })
}

fn qualified_output_references(part: &str, reject_unclosed: bool) -> Result<Vec<&str>> {
    let mut references = Vec::new();
    let mut rest = part;
    while let Some(start) = rest.find("${") {
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            if reject_unclosed {
                bail!("unclosed task output reference in `{part}`");
            }
            break;
        };
        let reference = &after_start[..end];
        if is_output_reference_syntax(reference) {
            references.push(reference);
        }
        rest = &after_start[end + 1..];
    }
    Ok(references)
}

fn interpolate_output_part(
    part: &str,
    outputs: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<String> {
    let mut result = String::new();
    let mut rest = part;
    while let Some(start) = rest.find("${") {
        result.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            bail!("unclosed task output reference in `{part}`");
        };
        let reference = &after_start[..end];
        if is_output_reference_syntax(reference) {
            let (task_name, output_name) = reference
                .rsplit_once('.')
                .expect("validated output reference must contain a dot");
            let value = outputs
                .get(task_name)
                .and_then(|task| task.get(output_name))
                .ok_or_else(|| anyhow!("task output `{reference}` is not available"))?;
            result.push_str(value);
        } else {
            result.push_str("${");
            result.push_str(reference);
            result.push('}');
        }
        rest = &after_start[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{AutoApply, TaskCommand, TaskRegistry};
    use crate::manifest::{Ecosystem, JsPackageManager, Package, PackageId};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolves_task_for_package_ecosystem() {
        let root = temp_dir("task-registry");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
cargo = ["cargo", "build"]
npm = ["npm", "run", "build"]
uv = ["uv", "run", "python", "-m", "build"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        let package = Package {
            id: PackageId::new(Ecosystem::Cargo, "crate-a"),
            name: "crate-a".into(),
            ecosystem: Ecosystem::Cargo,
            manifest_path: root.join("crate-a/Cargo.toml"),
            js_package_manager: None,
            task_opt_ins: Default::default(),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![],
        };

        let resolved = registry.resolve(&package, "build").expect("resolve task");
        assert_eq!(
            resolved.command,
            super::TaskCommand::Argv(vec!["cargo".into(), "build".into()])
        );
    }

    #[test]
    fn errors_when_ecosystem_variant_is_missing() {
        let root = temp_dir("missing-variant");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
cargo = ["cargo", "build"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        let package = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: root.join("web/package.json"),
            js_package_manager: Some(JsPackageManager::Npm),
            task_opt_ins: Default::default(),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![],
        };

        let err = registry
            .resolve(&package, "build")
            .expect_err("missing variant should fail");
        assert!(err.to_string().contains("has no variant"));
    }

    #[test]
    fn prefers_package_manager_specific_variant() {
        let root = temp_dir("specific-variant");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
npm = ["npm", "run", "build"]
pnpm = ["pnpm", "build"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        let package = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: root.join("web/package.json"),
            js_package_manager: Some(JsPackageManager::Pnpm),
            task_opt_ins: Default::default(),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![],
        };

        let resolved = registry.resolve(&package, "build").expect("resolve task");
        assert_eq!(
            resolved.command,
            super::TaskCommand::Argv(vec!["pnpm".into(), "build".into()])
        );
    }

    #[test]
    fn uses_default_command_when_ecosystem_variant_is_missing() {
        let root = temp_dir("default-task-command");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.docker]
default = ["docker", "build", "."]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        let package = Package {
            id: PackageId::new(Ecosystem::Uv, "py-app"),
            name: "py-app".into(),
            ecosystem: Ecosystem::Uv,
            manifest_path: root.join("py-app/pyproject.toml"),
            js_package_manager: None,
            task_opt_ins: Default::default(),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![],
        };

        let resolved = registry.resolve(&package, "docker").expect("resolve task");
        assert_eq!(
            resolved.command,
            super::TaskCommand::Argv(vec!["docker".into(), "build".into(), ".".into()])
        );
    }

    #[test]
    fn prefers_specific_variant_over_default_command() {
        let root = temp_dir("default-command-precedence");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
default = ["echo", "shared"]
npm = ["npm", "run", "build"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        let package = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: root.join("web/package.json"),
            js_package_manager: Some(JsPackageManager::Npm),
            task_opt_ins: Default::default(),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![],
        };

        let resolved = registry.resolve(&package, "build").expect("resolve task");
        assert_eq!(
            resolved.command,
            super::TaskCommand::Argv(vec!["npm".into(), "run".into(), "build".into()])
        );
    }

    #[test]
    fn reads_task_cascade_flag() {
        let root = temp_dir("task-cascade-flag");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.publish]
cascade = "all"
cargo = ["cargo", "publish"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        assert!(
            registry
                .cascades_to_native_dependencies("publish")
                .expect("cascade should load")
        );
        assert!(
            !registry
                .cascades_to_native_dependencies("build")
                .is_ok_and(|cascade| cascade)
        );
    }

    #[test]
    fn reads_task_dependencies() {
        let root = temp_dir("task-dependencies");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
depends_on = ["test", "lint"]
cargo = ["cargo", "build"]

[tasks.test]
cargo = ["cargo", "test"]

[tasks.lint]
cargo = ["cargo", "clippy"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        assert_eq!(
            registry
                .task_dependencies("build")
                .expect("task dependencies should load"),
            vec!["test".to_string(), "lint".to_string()]
        );
    }

    #[test]
    fn reads_workspace_batchable_flag() {
        let root = temp_dir("task-workspace-batchable");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.check]
workspace_batchable = true
cargo = ["cargo", "check"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        assert!(
            registry
                .workspace_batchable("check")
                .expect("workspace batchable should load")
        );
    }

    #[test]
    fn reads_ecosystem_scoped_task_dependencies() {
        let root = temp_dir("task-ecosystem-dependencies");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
npm = ["npm", "run", "build"]

[tasks.build.ecosystem_depends_on]
js = ["cargo:gen"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        assert_eq!(
            registry
                .ecosystem_task_dependencies("build", Ecosystem::Js)
                .expect("ecosystem dependencies should load"),
            vec![super::TargetedTaskDependency {
                ecosystem: Ecosystem::Cargo,
                task_name: "gen".to_string(),
            }]
        );
    }

    #[test]
    fn errors_on_unknown_task_fields() {
        let root = temp_dir("task-unknown-fields");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
autoaply = "inherit"
cargo = ["cargo", "build"]
"#,
        )
        .expect("write config");

        let err = TaskRegistry::load(&root).expect_err("unknown field should fail");
        assert!(err.to_string().contains("failed to parse task config"));
    }

    #[test]
    fn task_autoapply_defaults_to_none() {
        let root = temp_dir("task-autoapply-default");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.test]
cargo = ["cargo", "test"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        assert_eq!(
            registry
                .autoapply_mode("test")
                .expect("autoapply should load"),
            AutoApply::None
        );
    }

    #[test]
    fn reads_task_autoapply_inherit_mode() {
        let root = temp_dir("task-autoapply-flag");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.test]
autoapply = "inherit"
cargo = ["cargo", "test"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        assert_eq!(
            registry
                .autoapply_mode("test")
                .expect("autoapply should load"),
            AutoApply::Inherit
        );
    }

    #[test]
    fn errors_on_boolean_autoapply_values() {
        let root = temp_dir("task-autoapply-bool");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.test]
autoapply = false
cargo = ["cargo", "test"]
"#,
        )
        .expect("write config");

        let err = TaskRegistry::load(&root).expect_err("boolean autoapply should fail");
        assert!(err.to_string().contains("failed to parse task config"));
    }

    #[test]
    fn reads_task_autoapply_all_mode() {
        let root = temp_dir("task-autoapply-all");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.test]
autoapply = "all"
cargo = ["cargo", "test"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        assert_eq!(
            registry
                .autoapply_mode("test")
                .expect("autoapply should load"),
            AutoApply::All
        );
    }

    #[test]
    fn interpolates_task_variables() {
        let root = temp_dir("task-variable-interpolation");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
npm = ["npm", "run", "build:${mode}"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        let package = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: root.join("web/package.json"),
            js_package_manager: Some(JsPackageManager::Npm),
            task_opt_ins: BTreeMap::from([(
                "build".to_string(),
                crate::manifest::TaskOptIn {
                    variables: BTreeMap::from([("mode".to_string(), "production".to_string())]),
                },
            )]),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![],
        };

        let resolved = registry.resolve(&package, "build").expect("resolve task");
        assert_eq!(
            resolved.command,
            super::TaskCommand::Argv(vec!["npm".into(), "run".into(), "build:production".into()])
        );
    }

    #[test]
    fn errors_when_task_variable_is_missing() {
        let root = temp_dir("task-variable-missing");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
npm = ["npm", "run", "build:${mode}"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        let package = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: root.join("web/package.json"),
            js_package_manager: Some(JsPackageManager::Npm),
            task_opt_ins: BTreeMap::from([(
                "build".to_string(),
                crate::manifest::TaskOptIn::default(),
            )]),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![],
        };

        let err = registry
            .resolve(&package, "build")
            .expect_err("missing task variable should fail");
        assert!(
            err.to_string()
                .contains("task variable `mode` was not provided")
        );
    }

    #[test]
    fn errors_when_required_task_variable_is_not_supplied() {
        let root = temp_dir("task-variable-required");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.build]
variables = ["mode"]
npm = ["npm", "run", "build"]
"#,
        )
        .expect("write config");

        let registry = TaskRegistry::load(&root).expect("load registry");
        let package = Package {
            id: PackageId::new(Ecosystem::Js, "web"),
            name: "web".into(),
            ecosystem: Ecosystem::Js,
            manifest_path: root.join("web/package.json"),
            js_package_manager: Some(JsPackageManager::Npm),
            task_opt_ins: BTreeMap::from([(
                "build".to_string(),
                crate::manifest::TaskOptIn::default(),
            )]),
            bridged_dependencies: Default::default(),
            internal_dependencies: vec![],
        };

        let err = registry
            .resolve(&package, "build")
            .expect_err("missing required task variable should fail");
        assert!(err.to_string().contains(
            "package `web` opted into `build` but did not provide required task variable `mode`"
        ));
    }

    #[test]
    fn renders_task_with_variables() {
        let resolved = super::ResolvedTask {
            task_name: "build".to_string(),
            package_name: "web".to_string(),
            ecosystem: Ecosystem::Js,
            display_label: "npm".to_string(),
            explicitly_opted_in: true,
            package_dir: PathBuf::from("web"),
            variables: BTreeMap::from([("mode".to_string(), "production".to_string())]),
            command: super::TaskCommand::Argv(vec![
                "npm".to_string(),
                "run".to_string(),
                "build:production".to_string(),
            ]),
        };

        assert_eq!(resolved.render(), "web:build [npm] {mode=production}");
    }

    #[test]
    fn resolves_root_tasks_once_in_dependency_order() {
        let root = temp_dir("root-task-plan");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.prepare]
root = ["echo", "prepare"]

[tasks.release]
root = ["echo", "release"]
depends_on = ["prepare"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");

        let plan = registry.root_task_plan("release").expect("root task plan");
        let rendered = plan.iter().map(|task| task.render()).collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec!["workspace:prepare [root]", "workspace:release [root]"]
        );
    }

    #[test]
    fn package_tasks_can_depend_on_a_single_root_task() {
        let root = temp_dir("package-depends-root");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.prepare]
root = ["echo", "prepare"]

[tasks.check]
depends_on = ["prepare"]
autoapply = "all"
cargo = ["cargo", "check"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");

        let plan = registry
            .root_task_plan("check")
            .expect("root prerequisite plan");

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].task_name, "prepare");
    }

    #[test]
    fn root_task_dependency_cycles_fail() {
        let root = temp_dir("root-task-cycle");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.first]
root = ["echo", "first"]
depends_on = ["second"]

[tasks.second]
root = ["echo", "second"]
depends_on = ["first"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");

        let error = registry
            .root_task_plan("first")
            .expect_err("root cycle must fail");

        assert!(
            error
                .to_string()
                .contains("cycle detected in root task dependencies")
        );
    }

    #[test]
    fn root_tasks_reject_non_root_dependencies() {
        let root = temp_dir("root-depends-package");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.check]
autoapply = "all"
cargo = ["cargo", "check"]

[tasks.release]
root = ["echo", "release"]
depends_on = ["check"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");

        let error = registry
            .root_task_plan("release")
            .expect_err("cross-scope root dependency must fail");

        assert!(error.to_string().contains("has no root command"));
    }

    #[test]
    fn resolves_root_steps_outputs_and_condition() {
        let root = temp_dir("root-output-condition");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.probe]
root = ["printf", "1.2.3"]
outputs = { version = "stdout" }

[tasks.ship]
depends_on = ["probe"]
when = { output = "probe.version", nonempty = true }
root_steps = [["echo", "prepare"], ["echo", "${probe.version}"]]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = registry.root_task_plan("ship").expect("root plan");

        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].output_names, ["version"]);
        assert_eq!(plan[1].commands.len(), 2);
        assert_eq!(plan[1].depends_on, ["probe"]);
        let condition = plan[1].condition.as_ref().expect("condition");
        assert_eq!(condition.task_name, "probe");
        assert_eq!(condition.output_name, "version");
    }

    #[test]
    fn rejects_condition_on_non_dependency_output() {
        let root = temp_dir("non-dependency-output");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.probe]
root = ["printf", "1.2.3"]
outputs = { version = "stdout" }

[tasks.ship]
when = { output = "probe.version", nonempty = true }
root = ["echo", "ship"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let error = registry
            .root_task_plan("ship")
            .expect_err("invalid condition");
        assert!(error.to_string().contains("is not a dependency"));
    }

    #[test]
    fn rejects_root_and_root_steps_together() {
        let root = temp_dir("root-command-conflict");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.ship]
root = ["echo", "one"]
root_steps = [["echo", "two"]]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let error = registry
            .root_task_plan("ship")
            .expect_err("conflicting root commands");
        assert!(error.to_string().contains("both `root` and `root_steps`"));
    }

    #[test]
    fn output_interpolation_preserves_one_argv_element() {
        let command = TaskCommand::Argv(vec!["touch".into(), "v${probe.version}".into()]);
        let outputs = BTreeMap::from([(
            "probe".to_string(),
            BTreeMap::from([("version".to_string(), " 1; echo bad".to_string())]),
        )]);
        let resolved = command
            .interpolate_outputs(&outputs)
            .expect("interpolate output");
        assert_eq!(
            resolved,
            TaskCommand::Argv(vec!["touch".into(), "v 1; echo bad".into()])
        );
    }

    #[test]
    fn rejects_argv_output_reference_without_dependency() {
        let root = temp_dir("argv-output-nondependency");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.probe]
root = ["printf", "value"]
outputs = { value = "stdout" }

[tasks.consumer]
root = ["echo", "${probe.value}"]
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let error = registry
            .root_task_plan("consumer")
            .expect_err("implicit dependency should fail");
        assert!(error.to_string().contains("is not a dependency"));
    }

    #[test]
    fn rejects_output_reference_in_shell_command_before_execution() {
        let root = temp_dir("shell-output-reference");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.probe]
root = ["printf", "value"]
outputs = { value = "stdout" }

[tasks.consumer]
depends_on = ["probe"]
root = "echo ${probe.value}"
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let error = registry
            .root_task_plan("consumer")
            .expect_err("shell substitution should fail");
        assert!(error.to_string().contains("only in argv commands"));
    }

    #[test]
    fn rejects_ambiguous_output_names() {
        let root = temp_dir("ambiguous-output-name");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.probe]
root = ["printf", "value"]
outputs = { "release.version" = "stdout" }
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let error = registry
            .root_task_plan("probe")
            .expect_err("ambiguous output should fail");
        assert!(error.to_string().contains("must start with a letter"));
    }

    #[test]
    fn preserves_non_flux_braced_values_in_argv() {
        let command = TaskCommand::Argv(vec![
            "echo".into(),
            "${VERSION:-1.0}".into(),
            "${FILE:-foo.bar}".into(),
        ]);
        let resolved = command
            .interpolate_outputs(&BTreeMap::new())
            .expect("preserve ordinary values");
        assert_eq!(command, resolved);
    }

    #[test]
    fn preserves_normal_shell_parameter_defaults_with_dots() {
        let root = temp_dir("shell-parameter-default");
        fs::write(
            root.join("flux.toml"),
            r#"[tasks.render]
root = 'printf "%s" "${VERSION:-1.0}" "${FILE:-foo.bar}"'
"#,
        )
        .expect("write config");
        let registry = TaskRegistry::load(&root).expect("load registry");
        let plan = registry.root_task_plan("render").expect("root plan");
        assert_eq!(plan.len(), 1);
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should work")
            .as_millis();
        let path = std::env::temp_dir().join(format!("cargo-flux-{prefix}-{millis}"));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }
}
