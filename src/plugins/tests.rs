//! Exercise extension contracts with a provider that knows nothing about Cargo.
use super::*;
use crate::execution::{ExecutionObserver, Invocation, PreparedInvocation, execute_with_plugins};
use crate::snapshot::{SnapshotPlan, SnapshotPolicy, SnapshotSelection};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Observations {
    prepared: usize,
    lines: usize,
    finished: usize,
    dropped: usize,
    snapshot_held: bool,
}

struct FixturePlugin {
    state: Arc<Mutex<Observations>>,
    fail_observation: bool,
}

impl ManifestPlugin for FixturePlugin {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn discover_manifests(&self, _: &Path) -> Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }
    fn parse_manifest(&self, _: &Path) -> Result<RawPackage> {
        anyhow::bail!("no manifests")
    }

    fn prepare_execution(&self, invocation: &Invocation<'_>) -> Option<PreparedInvocation> {
        if invocation.program != "fixture-tool" {
            return None;
        }
        self.state.lock().unwrap().prepared += 1;
        let mut command = std::process::Command::new("git");
        command
            .args(invocation.args)
            .current_dir(invocation.cwd)
            .envs(invocation.variables);
        Some(PreparedInvocation {
            command,
            observer: Box::new(FixtureObserver {
                state: self.state.clone(),
                fail: self.fail_observation,
            }),
        })
    }

    fn snapshot_policy(&self, options: &[String]) -> Result<Box<dyn SnapshotPolicy>> {
        Ok(Box::new(FixturePolicy {
            state: self.state.clone(),
            escape: options.iter().any(|option| option == "escape"),
        }))
    }
}

struct FixtureObserver {
    state: Arc<Mutex<Observations>>,
    fail: bool,
}
impl ExecutionObserver for FixtureObserver {
    fn stdout(&mut self, _: &[u8]) -> bool {
        self.state.lock().unwrap().lines += 1;
        false
    }
    fn finish(&mut self) -> Result<()> {
        self.state.lock().unwrap().finished += 1;
        anyhow::ensure!(!self.fail, "fixture metadata destination unavailable");
        Ok(())
    }
}
impl Drop for FixtureObserver {
    fn drop(&mut self) {
        self.state.lock().unwrap().dropped += 1;
    }
}

struct FixturePolicy {
    state: Arc<Mutex<Observations>>,
    escape: bool,
}
struct FixtureSelection {
    state: Arc<Mutex<Observations>>,
    escape: bool,
}
impl SnapshotPolicy for FixturePolicy {
    fn prepare(&self, _: &Path, _: &[PathBuf]) -> Result<Box<dyn SnapshotSelection>> {
        self.state.lock().unwrap().snapshot_held = true;
        Ok(Box::new(FixtureSelection {
            state: self.state.clone(),
            escape: self.escape,
        }))
    }
}
impl SnapshotSelection for FixtureSelection {
    fn select(&mut self, candidates: &BTreeSet<PathBuf>) -> Result<SnapshotPlan> {
        assert!(self.state.lock().unwrap().snapshot_held);
        let files = if self.escape {
            BTreeSet::from([PathBuf::from("outside.txt")])
        } else {
            candidates
                .iter()
                .filter(|file| file.ends_with("keep"))
                .cloned()
                .collect()
        };
        Ok(SnapshotPlan {
            files,
            include_empty_directories: false,
        })
    }
}
impl Drop for FixtureSelection {
    fn drop(&mut self) {
        self.state.lock().unwrap().snapshot_held = false;
    }
}

fn fixture(fail_observation: bool) -> (Arc<Mutex<Observations>>, Vec<Box<dyn ManifestPlugin>>) {
    let state = Arc::new(Mutex::new(Observations::default()));
    let plugins: Vec<Box<dyn ManifestPlugin>> = vec![Box::new(FixturePlugin {
        state: state.clone(),
        fail_observation,
    })];
    (state, plugins)
}

#[test]
fn non_cargo_plugin_observes_execution_without_owning_process_lifecycle() -> Result<()> {
    for fail_observation in [false, true] {
        let (state, plugins) = fixture(fail_observation);
        let root = tempfile::tempdir()?;
        let task = TaskCommand::Argv(vec!["fixture-tool".into(), "--version".into()]);
        let output = execute_with_plugins(&task, root.path(), &Default::default(), true, &plugins)?;
        assert_eq!(output.as_deref(), Some(""));
        let state = state.lock().unwrap();
        assert_eq!(state.prepared, 1);
        assert!(state.lines > 0);
        assert_eq!(state.finished, 1);
        assert_eq!(state.dropped, 1);
    }
    Ok(())
}

#[test]
fn failed_tasks_do_not_commit_observations_and_release_guards() -> Result<()> {
    let (state, plugins) = fixture(false);
    let root = tempfile::tempdir()?;
    let task = TaskCommand::Argv(vec![
        "fixture-tool".into(),
        "flux-invalid-subcommand".into(),
    ]);
    assert!(execute_with_plugins(&task, root.path(), &Default::default(), true, &plugins).is_err());
    let state = state.lock().unwrap();
    assert_eq!(state.finished, 0);
    assert_eq!(state.dropped, 1);
    Ok(())
}

#[test]
fn declined_invocations_keep_their_output() -> Result<()> {
    let (state, plugins) = fixture(false);
    let root = tempfile::tempdir()?;
    let task = TaskCommand::Argv(vec!["git".into(), "--version".into()]);
    let output = execute_with_plugins(&task, root.path(), &Default::default(), true, &plugins)?;
    assert!(output.unwrap().starts_with("git version "));
    assert_eq!(state.lock().unwrap().prepared, 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn shell_tasks_bypass_plugin_observation() -> Result<()> {
    let (state, plugins) = fixture(false);
    let root = tempfile::tempdir()?;
    let task = TaskCommand::Shell("printf 'opaque output'".into());
    assert_eq!(
        execute_with_plugins(&task, root.path(), &Default::default(), true, &plugins)?,
        Some("opaque output".into())
    );
    assert_eq!(state.lock().unwrap().prepared, 0);
    Ok(())
}

#[test]
fn non_cargo_snapshot_policy_selects_files_without_changing_producer() -> Result<()> {
    let (state, plugins) = fixture(false);
    let source = tempfile::tempdir()?;
    let destination = tempfile::tempdir()?;
    let objects = tempfile::tempdir()?;
    let store = crate::cache::Store {
        directory: Some(objects.path().to_owned()),
        s3: None,
    };
    std::fs::create_dir_all(source.path().join("dist/empty"))?;
    std::fs::write(source.path().join("dist/keep"), "useful")?;
    std::fs::write(source.path().join("dist/obsolete"), "old")?;
    let policy = snapshot_policy_with(&plugins, "fixture", &[])?;
    crate::storage::save(
        &source.path().canonicalize()?,
        &store,
        "selected",
        &["dist".into()],
        Some(policy.as_ref()),
    )?;
    assert!(!state.lock().unwrap().snapshot_held);
    assert!(source.path().join("dist/obsolete").exists());
    assert!(source.path().join("dist/empty").exists());
    crate::storage::restore(
        &destination.path().canonicalize()?,
        &store,
        &["selected".into()],
        &["dist".into()],
        true,
        0,
    )?;
    assert_eq!(
        std::fs::read(destination.path().join("dist/keep"))?,
        b"useful"
    );
    assert!(!destination.path().join("dist/obsolete").exists());
    assert!(!destination.path().join("dist/empty").exists());
    Ok(())
}

#[test]
fn snapshot_policy_cannot_add_undeclared_files() -> Result<()> {
    let (state, plugins) = fixture(false);
    let source = tempfile::tempdir()?;
    std::fs::create_dir(source.path().join("dist"))?;
    std::fs::write(source.path().join("dist/keep"), "useful")?;
    std::fs::write(source.path().join("outside.txt"), "private")?;
    let policy = snapshot_policy_with(&plugins, "fixture", &["escape".into()])?;
    let error = crate::storage::save(
        &source.path().canonicalize()?,
        &Default::default(),
        "escape",
        &["dist".into()],
        Some(policy.as_ref()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("undeclared files"));
    assert!(!source.path().join(".flux/cache").exists());
    assert!(!state.lock().unwrap().snapshot_held);
    Ok(())
}

#[test]
fn unsupported_capabilities_and_invalid_options_fail_explicitly() {
    assert!(snapshot_policy("missing", &[]).is_err());
    assert!(snapshot_policy("npm", &[]).is_err());
    assert!(snapshot_policy("uv", &[]).is_err());
    assert!(snapshot_policy("cargo", &["unknown=value".into()]).is_err());
    assert!(snapshot_policy("cargo", &["manifest-path=".into()]).is_err());
    assert!(snapshot_policy("cargo", &["manifest-path".into()]).is_err());
}
