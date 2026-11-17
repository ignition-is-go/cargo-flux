//! Process lifecycle and output routing shared by all ecosystem plugins.
use crate::tasks::TaskCommand;
use anyhow::{Context, Result, ensure};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub(crate) struct Invocation<'a> {
    pub(crate) program: &'a str,
    pub(crate) args: &'a [String],
    pub(crate) cwd: &'a Path,
    pub(crate) variables: &'a BTreeMap<String, String>,
    pub(crate) capture_stdout: bool,
}

/// Plugins may shape an invocation and interpret its stdout protocol. They do
/// not spawn or wait on the task. Dropping the observer releases its guards.
pub(crate) struct PreparedInvocation {
    pub(crate) command: Command,
    pub(crate) observer: Box<dyn ExecutionObserver>,
}

pub(crate) trait ExecutionObserver {
    /// Return whether this line belongs in user-visible/captured task stdout.
    fn stdout(&mut self, line: &[u8]) -> bool;
    /// Called only after successful execution and output capture. Observation
    /// failures are diagnostic and cannot change a successful task's outcome.
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}

struct TaskChild(Child);
impl Drop for TaskChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) fn execute(
    task: &TaskCommand,
    cwd: &Path,
    variables: &BTreeMap<String, String>,
    capture_stdout: bool,
) -> Result<Option<String>> {
    execute_with_plugins(
        task,
        cwd,
        variables,
        capture_stdout,
        &crate::plugins::default_plugins(),
    )
}

pub(crate) fn execute_with_plugins(
    task: &TaskCommand,
    cwd: &Path,
    variables: &BTreeMap<String, String>,
    capture_stdout: bool,
    plugins: &[Box<dyn crate::plugins::ManifestPlugin>],
) -> Result<Option<String>> {
    let mut observer = None;
    let mut command = match task {
        TaskCommand::Shell(script) => {
            let mut command = Command::new("sh");
            command
                .arg("-lc")
                .arg(script)
                .current_dir(cwd)
                .envs(variables);
            command
        }
        TaskCommand::Argv(argv) => {
            let (program, args) = argv
                .split_first()
                .context("resolved task command was empty")?;
            let invocation = Invocation {
                program,
                args,
                cwd,
                variables,
                capture_stdout,
            };
            if let Some(prepared) = crate::plugins::prepare_execution_with(plugins, &invocation) {
                observer = Some(prepared.observer);
                prepared.command
            } else {
                let mut command = Command::new(program);
                command.args(args).current_dir(cwd).envs(variables);
                command
            }
        }
    };
    if observer.is_some() || capture_stdout {
        return execute_observed(&mut command, observer, cwd, capture_stdout);
    }
    ensure!(
        command.status()?.success(),
        "task execution failed in {}",
        cwd.display()
    );
    Ok(None)
}

fn execute_observed(
    command: &mut Command,
    mut observer: Option<Box<dyn ExecutionObserver>>,
    cwd: &Path,
    capture_stdout: bool,
) -> Result<Option<String>> {
    let mut child = TaskChild(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let mut captured = Vec::new();
    let mut output = BufReader::new(child.0.stdout.take().context("task stdout")?);
    let mut line = Vec::new();
    while output.read_until(b'\n', &mut line)? != 0 {
        if observer
            .as_mut()
            .is_none_or(|observer| observer.stdout(&line))
        {
            std::io::stdout().write_all(&line)?;
            if capture_stdout {
                captured.extend_from_slice(&line);
            }
        }
        line.clear();
    }
    ensure!(
        child.0.wait()?.success(),
        "task execution failed in {}",
        cwd.display()
    );
    let captured = if capture_stdout {
        if !captured.is_empty() && !captured.ends_with(b"\n") {
            writeln!(std::io::stdout())?;
        }
        Some(String::from_utf8(captured).context("task stdout was not UTF-8")?)
    } else {
        None
    };
    if let Some(observer) = observer.as_mut()
        && let Err(error) = observer.finish()
    {
        eprintln!("task observation unavailable: {error:#}");
    }
    Ok(captured)
}
