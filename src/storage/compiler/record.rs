//! Compiler-unit provenance reported by a successful Cargo invocation.
use super::*;
use std::io::{BufRead, BufReader};
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
    pub(super) workspace: PathBuf,
    pub(super) package: String,
    pub(super) fingerprints: BTreeSet<u64>,
}

pub(in crate::storage) fn lock(root: &Path, paths: &[PathBuf]) -> Result<Vec<fs::File>> {
    let directory = root.join(".flux/locks/compiler");
    let mut ancestor = directory.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .context("compiler lock directory parent")?;
    }
    ensure!(
        ancestor.canonicalize()?.starts_with(root),
        "compiler lock directory leaves workspace"
    );
    fs::create_dir_all(&directory)?;
    ensure!(
        directory.canonicalize()?.starts_with(root),
        "compiler lock directory leaves workspace"
    );
    let mut locks = Vec::new();
    for path in paths.iter().collect::<BTreeSet<_>>() {
        let name = format!(
            "{:x}.lock",
            Sha256::digest(path.to_string_lossy().as_bytes())
        );
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(name))?;
        file.lock()
            .with_context(|| format!("locking compiler cache: {}", path.display()))?;
        locks.push(file);
    }
    Ok(locks)
}

pub(super) fn load(root: &Path, paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, Record>> {
    let mut records = BTreeMap::new();
    for path in paths {
        match fs::File::open(root.join(path).join(FILE)) {
            Ok(file) => records.extend(serde_json::from_reader::<_, BTreeMap<PathBuf, Record>>(
                file,
            )?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
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

pub(in crate::storage) fn run(root: &Path, paths: &[PathBuf], args: &[String]) -> Result<()> {
    let _locks = lock(root, paths)?;
    let mut arguments = args.to_vec();
    let position = arguments
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(arguments.len());
    let format = arguments[..position]
        .iter()
        .enumerate()
        .find_map(|(index, arg)| {
            arg.strip_prefix("--message-format=")
                .map(str::to_owned)
                .or_else(|| {
                    (arg == "--message-format")
                        .then(|| arguments.get(index + 1).cloned())
                        .flatten()
                })
        });
    let forward_json = format.is_some();
    if let Some(format) = format {
        ensure!(
            format.split(',').any(|part| part.starts_with("json")),
            "storage cargo requires a JSON message format"
        );
    } else {
        arguments.insert(position, "--message-format=json-render-diagnostics".into());
    }
    let mut child = CargoChild(
        Command::new("cargo")
            .current_dir(root)
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let mut output = BufReader::new(child.0.stdout.take().context("Cargo stdout")?);
    let mut artifacts = Vec::new();
    let mut line = Vec::new();
    while output.read_until(b'\n', &mut line)? != 0 {
        let message = serde_json::from_slice::<serde_json::Value>(&line).ok();
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
        if forward_json || !cargo_message {
            std::io::stdout().write_all(&line)?;
        }
        if let Some(message) = message
            && message["reason"] == "compiler-artifact"
        {
            let artifact: Artifact = serde_json::from_value(message)?;
            if artifact.package_id.starts_with("path+") {
                artifacts.push(artifact);
            }
        }
        line.clear();
    }
    ensure!(
        child.0.wait()?.success(),
        "Cargo compilation failed; compiler provenance was not updated"
    );
    ensure!(
        !artifacts.is_empty(),
        "Cargo emitted no local compiler artifacts; use a JSON message format"
    );
    let mut locate = Command::new("cargo");
    locate
        .current_dir(root)
        .args(["locate-project", "--workspace", "--message-format=plain"]);
    for (index, arg) in args.iter().enumerate() {
        if let Some(path) = arg.strip_prefix("--manifest-path=") {
            locate.arg("--manifest-path").arg(path);
        } else if arg == "--manifest-path" {
            locate
                .arg("--manifest-path")
                .arg(args.get(index + 1).context("Cargo manifest path")?);
        }
        if arg == "--" {
            break;
        }
    }
    let project = locate.output()?;
    ensure!(project.status.success(), "Cargo workspace location failed");
    let manifest = PathBuf::from(String::from_utf8(project.stdout)?.trim());
    let workspace = manifest
        .parent()
        .context("Cargo workspace parent")?
        .canonicalize()?;
    record(root, paths, artifacts, &workspace)
}

fn record(
    root: &Path,
    paths: &[PathBuf],
    artifacts: Vec<Artifact>,
    workspace: &Path,
) -> Result<()> {
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
    let mut records = load(root, paths)?;
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
                    workspace: workspace.to_owned(),
                    package: artifact.package_id.clone(),
                    fingerprints,
                },
            );
        }
    }
    for path in paths {
        let selected = records
            .iter()
            .filter(|(key, _)| key.starts_with(path))
            .collect::<BTreeMap<_, _>>();
        let mut pending = tempfile::NamedTempFile::new_in(root.join(path))?;
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
