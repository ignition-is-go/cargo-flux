//! Compiler-unit provenance reported by a successful Cargo invocation.
use super::*;
use std::io::{BufRead, BufReader, IsTerminal};
use std::process::Stdio;

const FILE: &str = ".flux-compiler-units.json";

struct CargoChild(std::process::Child);

impl Drop for CargoChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Record {
    pub(super) manifest: PathBuf,
    pub(super) package: String,
    pub(super) fingerprints: BTreeSet<u64>,
}

pub(super) const LOCK: &str = ".flux-compiler.lock";

pub(in crate::storage) fn lock(root: &Path, paths: &[PathBuf]) -> Result<Vec<fs::File>> {
    let mut locks = Vec::new();
    for path in paths.iter().collect::<BTreeSet<_>>() {
        let directory = root.join(path);
        if directory.is_file() {
            continue;
        }
        let mut ancestor = directory.as_path();
        while !ancestor.exists() {
            ancestor = ancestor.parent().context("compiler cache parent")?;
        }
        ensure!(
            ancestor.canonicalize()?.starts_with(root),
            "compiler cache leaves workspace"
        );
        fs::create_dir_all(&directory)?;
        ensure!(
            !directory
                .join(LOCK)
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.file_type().is_symlink()),
            "compiler lock must not be a symlink"
        );
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(LOCK))?;
        file.lock().context("locking compiler cache")?;
        locks.push(file);
    }
    Ok(locks)
}

pub(super) fn load(root: &Path, paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, Record>> {
    let mut records = BTreeMap::new();
    for path in paths {
        match fs::File::open(root.join(path).join(FILE)) {
            Ok(file) => {
                let saved: BTreeMap<PathBuf, Record> = serde_json::from_reader(file)?;
                for (key, record) in saved {
                    relative(&key)?;
                    records.insert(path.join(key), record);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(records)
}

#[derive(Deserialize)]
struct Artifact {
    package_id: String,
    manifest_path: PathBuf,
    filenames: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct CargoMetadata {
    workspace_root: PathBuf,
    target_directory: PathBuf,
    build_directory: Option<PathBuf>,
}

pub(crate) struct Recording {
    command: Command,
    cwd: PathBuf,
    root: PathBuf,
    paths: Vec<PathBuf>,
    forward_json: bool,
    _locks: Vec<fs::File>,
}

fn future_canonical(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        suffix.push(
            ancestor
                .file_name()
                .context("compiler cache name")?
                .to_owned(),
        );
        ancestor = ancestor.parent().context("compiler cache parent")?;
    }
    let mut resolved = ancestor.canonicalize()?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

pub(in crate::storage) fn prepare(
    program: &str,
    args: &[String],
    cwd: &Path,
    variables: &BTreeMap<String, String>,
    capture_stdout: bool,
) -> Option<Recording> {
    if !capture_stdout && std::io::stdout().is_terminal() {
        return None;
    }
    if Path::new(program).file_stem()?.to_str()? != "cargo"
        || !matches!(
            args.first()?.as_str(),
            "build" | "check" | "test" | "bench" | "clippy" | "rustc"
        )
    {
        return None;
    }
    let position = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let format = args[..position]
        .iter()
        .enumerate()
        .find_map(|(index, arg)| {
            arg.strip_prefix("--message-format=")
                .map(str::to_owned)
                .or_else(|| {
                    (arg == "--message-format")
                        .then(|| args.get(index + 1).cloned())
                        .flatten()
                })
        });
    if format
        .as_ref()
        .is_some_and(|format| !format.split(',').any(|part| part.starts_with("json")))
    {
        return None;
    }
    let mut metadata = Command::new(program);
    metadata.current_dir(cwd).envs(variables).args([
        "metadata",
        "--no-deps",
        "--offline",
        "--locked",
        "--format-version=1",
    ]);
    let mut target = None;
    for (index, arg) in args[..position].iter().enumerate() {
        if let Some(path) = arg.strip_prefix("--target-dir=") {
            target = Some(cwd.join(path));
        }
        if arg == "--target-dir" {
            target = Some(cwd.join(args.get(index + 1)?));
        }
        for option in ["--manifest-path", "--config"] {
            if arg == option {
                metadata.arg(option).arg(args.get(index + 1)?);
            }
            if arg.starts_with(&format!("{option}=")) {
                metadata.arg(arg);
            }
        }
    }
    if let Some(target) = target {
        metadata.env("CARGO_TARGET_DIR", target);
    }
    let output = metadata.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let metadata: CargoMetadata = serde_json::from_slice(&output.stdout).ok()?;
    let workspace = metadata.workspace_root.canonicalize().ok()?;
    let mut absolute = vec![future_canonical(&metadata.target_directory).ok()?];
    if let Some(build) = metadata.build_directory {
        absolute.push(future_canonical(&build).ok()?);
    }
    let mut root = cwd.canonicalize().ok()?;
    for path in absolute.iter().chain(std::iter::once(&workspace)) {
        while !path.starts_with(&root) {
            root = root.parent()?.to_owned();
        }
    }
    let mut paths = absolute
        .iter()
        .map(|path| path.strip_prefix(&root).ok().map(Path::to_owned))
        .collect::<Option<Vec<_>>>()?;
    paths.sort();
    paths.dedup();
    if validate_paths(&paths).is_err() {
        return None;
    }
    let locks = match lock(&root, &paths) {
        Ok(locks) => locks,
        Err(error) => {
            eprintln!("compiler provenance unavailable: {error}");
            return None;
        }
    };
    let mut arguments = args.to_vec();
    if format.is_none() {
        arguments.insert(position, "--message-format=json-render-diagnostics".into());
    }
    let mut command = Command::new(program);
    command.current_dir(cwd).envs(variables).args(arguments);
    Some(Recording {
        command,
        cwd: cwd.to_owned(),
        root,
        paths,
        forward_json: format.is_some(),
        _locks: locks,
    })
}

impl Recording {
    pub(crate) fn execute(mut self, capture_stdout: bool) -> Result<Option<String>> {
        let mut child = CargoChild(
            self.command
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
        let mut captured = Vec::new();
        let mut artifact_error = None;
        let mut compilation_finished = false;
        let mut output = BufReader::new(child.0.stdout.take().context("Cargo stdout")?);
        let mut artifacts = Vec::new();
        let mut line = Vec::new();
        while output.read_until(b'\n', &mut line)? != 0 {
            let message = if compilation_finished {
                None
            } else {
                serde_json::from_slice::<serde_json::Value>(&line).ok()
            };
            if message
                .as_ref()
                .is_some_and(|message| message["reason"] == "build-finished")
            {
                compilation_finished = true;
            }
            let cargo_message = message
                .as_ref()
                .and_then(|message| message["reason"].as_str())
                .is_some_and(|reason| {
                    matches!(
                        reason,
                        "compiler-artifact"
                            | "build-script-executed"
                            | "build-finished"
                            | "compiler-message"
                    )
                });
            if self.forward_json || !cargo_message {
                std::io::stdout().write_all(&line)?;
                if capture_stdout {
                    captured.extend_from_slice(&line);
                }
            }
            if let Some(message) = message
                && message["reason"] == "compiler-artifact"
            {
                match serde_json::from_value::<Artifact>(message) {
                    Ok(artifact) if artifact.package_id.starts_with("path+") => {
                        artifacts.push(artifact)
                    }
                    Ok(_) => (),
                    Err(error) => artifact_error = Some(error),
                }
            }
            line.clear();
        }
        ensure!(
            child.0.wait()?.success(),
            "task execution failed in {}",
            self.cwd.display()
        );
        let captured = if capture_stdout {
            if !captured.is_empty() && !captured.ends_with(b"\n") {
                writeln!(std::io::stdout())?;
            }
            Some(String::from_utf8(captured).context("task stdout was not UTF-8")?)
        } else {
            None
        };
        if let Some(error) = artifact_error {
            eprintln!("compiler provenance unavailable: {error}");
        } else if !artifacts.is_empty()
            && let Err(error) = record(&self.root, &self.paths, artifacts)
        {
            eprintln!("compiler provenance unavailable: {error}");
        }
        Ok(captured)
    }
}

fn record(root: &Path, paths: &[PathBuf], artifacts: Vec<Artifact>) -> Result<()> {
    let manifests = artifacts
        .iter()
        .map(|artifact| artifact.manifest_path.clone())
        .collect::<BTreeSet<_>>();
    let sources = sources::Sources::load(root, &manifests.into_iter().collect::<Vec<_>>())?;
    let mut files = BTreeSet::new();
    for path in paths {
        collect(root, path, &mut files)?;
    }
    let profiles = files
        .iter()
        .filter_map(|path| fingerprint_location(path).map(|value| value.0))
        .collect::<BTreeSet<_>>();
    // Capture identities while Cargo's output aliases still point at this invocation's units.
    let _locks = lock_profiles(root, &profiles, &mut files)?;
    let mut units = BTreeMap::<(PathBuf, String), BTreeSet<u64>>::new();
    let mut identities =
        std::collections::HashMap::<file_id::FileId, BTreeSet<(PathBuf, String)>>::new();
    for path in &files {
        if let Some(id) = unit_id(path, &profiles) {
            identities
                .entry(file_id::get_file_id(root.join(path))?)
                .or_default()
                .insert(id.clone());
            let values = units.entry(id).or_default();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
                && fingerprint_location(path).is_some()
            {
                let stamp = fs::read_to_string(root.join(path.with_extension("")))?;
                if let Some(value) = fingerprint_value(&stamp) {
                    values.insert(value);
                }
            }
        }
    }
    let mut records = load(root, paths).unwrap_or_else(|error| {
        eprintln!("compiler provenance unavailable: {error}");
        BTreeMap::new()
    });
    let current = units
        .keys()
        .map(|(profile, hash)| profile.join(hash))
        .collect::<BTreeSet<_>>();
    records.retain(|key, _| current.contains(key));
    for artifact in artifacts {
        let manifest = artifact.manifest_path.canonicalize()?;
        ensure!(
            sources.current_identity(&manifest, &artifact.package_id) == Some(true),
            "Cargo artifact package identity does not match its manifest"
        );
        let mut reported = BTreeSet::new();
        for file in artifact.filenames {
            if let Some(ids) = identities.get(&file_id::get_file_id(file)?) {
                reported.extend(ids.iter().cloned());
            }
        }
        ensure!(
            reported.len() == 1,
            "Cargo artifact must identify one compiler unit in the declared cache paths"
        );
        for id in reported {
            let fingerprints = units[&id].clone();
            ensure!(
                !fingerprints.is_empty(),
                "Cargo artifact has no completed fingerprint"
            );
            records.insert(
                id.0.join(id.1),
                Record {
                    manifest: manifest.clone(),
                    package: artifact.package_id.clone(),
                    fingerprints,
                },
            );
        }
    }
    for path in paths {
        let selected = records
            .iter()
            .filter_map(|(key, record)| key.strip_prefix(path).ok().map(|key| (key, record)))
            .collect::<BTreeMap<_, _>>();
        let builder = tempfile::Builder::new();
        #[cfg(unix)]
        let builder = {
            let mut builder = builder;
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o666));
            builder
        };
        let mut pending = builder.tempfile_in(root.join(path))?;
        serde_json::to_writer(pending.as_file_mut(), &selected)?;
        pending.persist(root.join(path).join(FILE))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_cache_lock_serializes_owners() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let paths = vec![PathBuf::from("target")];
        let first = lock(&root, &paths)?;
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::scope(|scope| -> Result<()> {
            let worker = scope.spawn(|| -> Result<()> {
                sender.send(false)?;
                let _second = lock(&root, &paths)?;
                sender.send(true)?;
                Ok(())
            });
            assert!(!receiver.recv()?);
            assert!(receiver.try_recv().is_err());
            drop(first);
            assert!(receiver.recv_timeout(std::time::Duration::from_secs(10))?);
            worker.join().unwrap()?;
            Ok(())
        })
    }
}
