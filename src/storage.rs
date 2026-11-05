//! Explicit cross-job artifacts and reusable compiler snapshots. Restoring these
//! files never records a task as passed; callers still execute their validation.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::cache::Store;

const HASH_WORKERS: usize = 8;
const MANIFEST: &str = "__flux_storage_manifest.json";

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    key: String,
    paths: Vec<PathBuf>,
    files: BTreeMap<PathBuf, FileState>,
}

#[derive(Serialize, Deserialize, Clone)]
struct FileState {
    digest: String,
    seconds: u64,
    nanos: u32,
}

fn file_state(path: &Path) -> Result<FileState> {
    let modified = fs::metadata(path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?;
    Ok(FileState {
        digest: digest(path)?,
        seconds: modified.as_secs(),
        nanos: modified.subsec_nanos(),
    })
}

fn hash_workers(files: usize) -> usize {
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(HASH_WORKERS)
        .min(files)
}

/// Bound filesystem hashing independently of Cargo's compiler job count.
fn map_files<T: Send>(
    paths: &BTreeSet<PathBuf>,
    operation: impl Fn(&Path) -> Result<T> + Sync,
) -> Result<BTreeMap<PathBuf, T>> {
    let paths = paths.iter().collect::<Vec<_>>();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (send, receive) = std::sync::mpsc::channel();
    let workers = hash_workers(paths.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let paths = &paths;
            let next = &next;
            let send = send.clone();
            let operation = &operation;
            scope.spawn(move || {
                while let Some(path) =
                    paths.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                {
                    if send
                        .send(operation(path).map(|value| ((*path).clone(), value)))
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
    });
    drop(send);
    receive.into_iter().collect()
}

fn phase(name: &str, started: std::time::Instant) {
    eprintln!("storage {name}: {:.2}s", started.elapsed().as_secs_f64());
}

fn relative(path: &Path) -> Result<()> {
    ensure!(!path.as_os_str().is_empty(), "empty storage path");
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(_))),
        "storage paths must be relative without traversal"
    );
    ensure!(
        !path.starts_with(".git") && path != Path::new(MANIFEST),
        "reserved storage path"
    );
    Ok(())
}

fn digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn collect(root: &Path, path: &Path, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    relative(path)?;
    let metadata = fs::symlink_metadata(root.join(path))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "storage symlinks are not supported"
    );
    if metadata.is_dir() {
        for child in fs::read_dir(root.join(path))? {
            collect(root, &path.join(child?.file_name()), files)?;
        }
    } else {
        ensure!(
            metadata.is_file(),
            "storage supports regular files and directories only"
        );
        files.insert(path.to_owned());
    }
    Ok(())
}

fn validate_paths(paths: &[PathBuf]) -> Result<()> {
    ensure!(!paths.is_empty(), "declare at least one storage path");
    for (index, path) in paths.iter().enumerate() {
        relative(path)?;
        ensure!(
            !paths[..index]
                .iter()
                .any(|other| path.starts_with(other) || other.starts_with(path)),
            "storage paths overlap"
        );
    }
    Ok(())
}

fn object_key(key: &str) -> Result<String> {
    ensure!(!key.trim().is_empty(), "empty storage key");
    Ok(format!("objects/{:x}", Sha256::digest(key.as_bytes())))
}

fn transfer(root: &Path, store: &Store, key: &str, upload: bool, file: &Path) -> Result<bool> {
    let key = object_key(key)?;
    if let Some(config) = &store.s3 {
        return crate::cache::s3::transfer(
            config,
            &key,
            if upload { "upload" } else { "download" },
            file,
        );
    }
    let destination = root
        .join(
            store
                .directory
                .as_deref()
                .unwrap_or(Path::new(".flux/cache")),
        )
        .join(format!("{key}.tar"));
    if upload {
        let parent = destination.parent().context("storage parent")?;
        fs::create_dir_all(parent)?;
        let mut pending = tempfile::NamedTempFile::new_in(parent)?;
        std::io::copy(&mut fs::File::open(file)?, &mut pending)?;
        pending.persist(destination)?;
        Ok(true)
    } else {
        match fs::copy(destination, file) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

pub fn save(root: &Path, store: &Store, key: &str, paths: &[PathBuf]) -> Result<()> {
    validate_paths(paths)?;
    let mut files = BTreeSet::new();
    for path in paths {
        ensure!(
            root.join(path).canonicalize()?.starts_with(root),
            "storage path leaves workspace"
        );
        collect(root, path, &mut files)?;
    }
    let started = std::time::Instant::now();
    eprintln!(
        "storage hashing {} files ({} workers)",
        files.len(),
        hash_workers(files.len())
    );
    let hashes = map_files(&files, |path| file_state(&root.join(path)))?;
    phase("hash", started);
    let manifest = serde_json::to_vec(&Manifest {
        version: 1,
        key: key.into(),
        paths: paths.to_vec(),
        files: hashes.clone(),
    })?;
    let mut temporary = tempfile::NamedTempFile::new()?;
    let started = std::time::Instant::now();
    eprintln!("storage compressing archive");
    {
        let mut encoder = zstd::stream::Encoder::new(&mut temporary, 1)?;
        encoder.multithread(
            std::thread::available_parallelism()
                .map_or(1, usize::from)
                .min(4) as u32,
        )?;
        let mut archive = tar::Builder::new(encoder);
        // Keep sparse linker outputs sparse in the archive and on restoration.
        archive.sparse(true);
        archive.follow_symlinks(false);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(manifest.len() as u64);
        header.set_cksum();
        archive.append_data(&mut header, MANIFEST, &manifest[..])?;
        for path in paths {
            if root.join(path).is_dir() {
                archive.append_dir_all(path, root.join(path))?;
            } else {
                archive.append_path_with_name(root.join(path), path)?;
            }
        }
        archive.into_inner()?.finish()?;
    }
    temporary.flush()?;
    phase("compress", started);
    let started = std::time::Instant::now();
    eprintln!("storage verifying snapshot stability");
    // Recheck producer inputs without expanding a second target tree: a Cargo
    // snapshot can be hundreds of gigabytes of sparse files.
    let mut current = BTreeSet::new();
    for path in paths {
        collect(root, path, &mut current)?;
    }
    ensure!(
        current == hashes.keys().cloned().collect(),
        "storage files changed during upload preparation"
    );
    map_files(&current, |path| {
        ensure!(
            digest(&root.join(path))? == hashes[path].digest,
            "storage contents changed during upload preparation"
        );
        Ok(())
    })?;
    phase("verify source", started);
    let bytes = temporary.as_file().metadata()?.len();
    let started = std::time::Instant::now();
    eprintln!("storage uploading {bytes} compressed bytes");
    transfer(root, store, key, true, temporary.path())?;
    phase("upload", started);
    Ok(())
}

fn unpack(archive: &Path, stage: &Path, key: &str, paths: &[PathBuf]) -> Result<()> {
    let started = std::time::Instant::now();
    eprintln!("storage extracting archive");
    let decoder = zstd::stream::Decoder::new(fs::File::open(archive)?)?;
    let mut archive = tar::Archive::new(decoder);
    let mut entries = archive.entries()?;
    let mut first = entries.next().context("empty storage archive")??;
    ensure!(
        first.path()?.as_ref() == Path::new(MANIFEST),
        "missing storage manifest"
    );
    ensure!(
        first.size() <= 64 * 1024 * 1024,
        "oversized storage manifest"
    );
    let manifest: Manifest = serde_json::from_reader(&mut first)?;
    ensure!(
        manifest.version == 1 && manifest.key == key && manifest.paths == paths,
        "storage identity or paths mismatch"
    );
    let mut seen = BTreeSet::new();
    for entry in entries {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        relative(&path)?;
        ensure!(seen.insert(path.clone()), "duplicate storage entry");
        ensure!(
            paths.iter().any(|output| path.starts_with(output)),
            "undeclared storage output"
        );
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir() || kind.is_gnu_sparse(),
            "storage links and special files are forbidden"
        );
        ensure!(entry.unpack_in(stage)?, "invalid storage entry");
    }
    phase("extract", started);
    let started = std::time::Instant::now();
    eprintln!(
        "storage verifying {} files ({} workers)",
        manifest.files.len(),
        hash_workers(manifest.files.len())
    );
    let mut actual = BTreeSet::new();
    for path in paths {
        collect(stage, path, &mut actual)?;
    }
    ensure!(
        actual == manifest.files.keys().cloned().collect(),
        "storage file list mismatch"
    );
    map_files(&actual, |path| {
        let expected = &manifest.files[path];
        ensure!(
            digest(&stage.join(path))? == expected.digest,
            "storage checksum mismatch"
        );
        fs::File::open(stage.join(path))?.set_times(fs::FileTimes::new().set_modified(
            std::time::UNIX_EPOCH + std::time::Duration::new(expected.seconds, expected.nanos),
        ))?;
        Ok(())
    })?;
    phase("verify outputs", started);
    Ok(())
}

pub fn restore(
    root: &Path,
    store: &Store,
    keys: &[String],
    paths: &[PathBuf],
    required: bool,
) -> Result<bool> {
    validate_paths(paths)?;
    ensure!(!keys.is_empty(), "declare at least one storage key");
    let temporary = tempfile::NamedTempFile::new()?;
    for key in keys {
        let started = std::time::Instant::now();
        eprintln!("storage downloading archive");
        if !transfer(root, store, key, false, temporary.path())? {
            continue;
        }
        let bytes = temporary.as_file().metadata()?.len();
        let seconds = started.elapsed().as_secs_f64();
        eprintln!(
            "storage download: {bytes} bytes in {seconds:.2}s ({:.1} MiB/s)",
            bytes as f64 / (1024.0 * 1024.0) / seconds.max(0.001)
        );
        // Same filesystem staging permits rename without dense copies of sparse
        // files. Every byte is checked before replacing any existing output.
        let stage = tempfile::tempdir_in(root)?;
        unpack(temporary.path(), stage.path(), key, paths)?;
        for path in paths {
            let output = root.join(path);
            let parent = output.parent().context("storage output parent")?;
            let mut ancestor = parent;
            while !ancestor.exists() {
                ancestor = ancestor.parent().context("storage output ancestor")?;
            }
            ensure!(
                ancestor.canonicalize()?.starts_with(root),
                "storage output parent leaves workspace"
            );
            fs::create_dir_all(parent)?;
            match output.symlink_metadata() {
                Ok(metadata) => {
                    ensure!(
                        !metadata.file_type().is_symlink(),
                        "refusing to replace symlink output"
                    );
                    if metadata.is_dir() {
                        fs::remove_dir_all(&output)?;
                    } else {
                        fs::remove_file(&output)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
            fs::rename(stage.path().join(path), output)?;
        }
        println!("storage hit: {key}");
        return Ok(true);
    }
    ensure!(!required, "required storage artifact was not found");
    println!("storage miss");
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archives_restore_across_roots_replace_stale_files_and_try_fallbacks() -> Result<()> {
        let producer = tempfile::tempdir()?;
        let consumer = tempfile::tempdir()?;
        let objects = tempfile::tempdir()?;
        let store = Store {
            directory: Some(objects.path().to_owned()),
            s3: None,
        };
        fs::create_dir(producer.path().join("dist"))?;
        fs::write(producer.path().join("dist/binary"), "compiled")?;
        let paths = vec![PathBuf::from("dist")];
        save(producer.path(), &store, "artifact/run/one", &paths)?;
        fs::create_dir(consumer.path().join("dist"))?;
        fs::write(consumer.path().join("dist/stale"), "stale")?;
        assert!(restore(
            consumer.path(),
            &store,
            &["absent".into(), "artifact/run/one".into()],
            &paths,
            true
        )?);
        assert_eq!(fs::read(consumer.path().join("dist/binary"))?, b"compiled");
        assert!(!consumer.path().join("dist/stale").exists());
        assert!(!restore(
            consumer.path(),
            &store,
            &["absent".into()],
            &paths,
            false
        )?);
        assert!(restore(consumer.path(), &store, &["absent".into()], &paths, true).is_err());
        assert!(
            restore(
                consumer.path(),
                &store,
                &["artifact/run/one".into()],
                &["other".into()],
                true
            )
            .is_err()
        );
        assert_eq!(fs::read(consumer.path().join("dist/binary"))?, b"compiled");
        Ok(())
    }

    #[test]
    fn corrupt_archives_do_not_modify_existing_outputs() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::default();
        fs::create_dir(root.path().join("dist"))?;
        fs::write(root.path().join("dist/binary"), "original")?;
        save(root.path(), &store, "one", &["dist".into()])?;
        let object = root
            .path()
            .join(format!(".flux/cache/{}.tar", object_key("one")?));
        let bytes = fs::read(&object)?;
        fs::write(object, &bytes[..bytes.len() / 2])?;
        assert!(restore(root.path(), &store, &["one".into()], &["dist".into()], true).is_err());
        assert_eq!(fs::read(root.path().join("dist/binary"))?, b"original");
        assert!(save(root.path(), &store, "bad", &["../outside".into()]).is_err());
        assert!(
            save(
                root.path(),
                &store,
                "bad",
                &["dist".into(), "dist/binary".into()]
            )
            .is_err()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn sparse_snapshots_keep_holes_modes_and_timestamps() -> Result<()> {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("target"))?;
        let path = root.path().join("target/linked-binary");
        let mut file = fs::File::create(&path)?;
        file.seek(SeekFrom::Start(64 * 1024 * 1024))?;
        file.write_all(b"end")?;
        file.set_permissions(fs::Permissions::from_mode(0o755))?;
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        file.set_times(fs::FileTimes::new().set_modified(time))?;
        drop(file);
        let before = fs::metadata(&path)?;
        let store = Store::default();
        save(root.path(), &store, "snapshot", &["target".into()])?;
        fs::remove_dir_all(root.path().join("target"))?;
        restore(
            root.path(),
            &store,
            &["snapshot".into()],
            &["target".into()],
            true,
        )?;
        let after = fs::metadata(&path)?;
        assert_eq!(after.len(), before.len());
        assert_eq!(after.permissions().mode() & 0o777, 0o755);
        assert_eq!(after.modified()?, time);
        assert!(
            after.blocks() * 512 < after.len() / 2,
            "sparse archive became dense"
        );
        assert!(
            fs::metadata(
                root.path()
                    .join(format!(".flux/cache/{}.tar", object_key("snapshot")?))
            )?
            .len()
                < 1024 * 1024
        );
        Ok(())
    }
}

/// Source contents deliberately do not enter compiler compatibility. Cargo's
/// fingerprints own source freshness, including manifests and dependency locks.
pub fn cargo_key(root: &Path, scope: &str, packages: &[String]) -> Result<String> {
    ensure!(
        !scope.is_empty() && !scope.contains(['\n', '\r']),
        "invalid Cargo cache scope"
    );
    let output = std::process::Command::new("rustc")
        .args(["--version", "--verbose"])
        .output()?;
    ensure!(output.status.success(), "rustc version probe failed");
    let flags = std::env::vars()
        .filter(|(name, _)| {
            matches!(
                name.as_str(),
                "RUSTFLAGS"
                    | "RUSTDOCFLAGS"
                    | "CARGO_ENCODED_RUSTFLAGS"
                    | "CARGO_BUILD_TARGET"
                    | "CC"
                    | "CXX"
                    | "CFLAGS"
                    | "CXXFLAGS"
                    | "RUSTC"
                    | "RUSTC_WRAPPER"
                    | "RUSTC_WORKSPACE_WRAPPER"
            ) || name.starts_with("CARGO_PROFILE_")
                || name.starts_with("CARGO_TARGET_")
        })
        .collect::<BTreeMap<_, _>>();
    let identity = serde_json::to_vec(&(
        std::env::consts::OS,
        std::env::consts::ARCH,
        root,
        scope,
        packages,
        output.stdout,
        flags,
    ))?;
    Ok(format!("cargo/{:x}", Sha256::digest(identity)))
}

pub fn restore_into(
    root: &Path,
    store: &Store,
    keys: &[String],
    paths: &[PathBuf],
    required: bool,
    destination: &Path,
) -> Result<bool> {
    if destination == Path::new(".") {
        return restore(root, store, keys, paths, required);
    }
    relative(destination)?;
    let output = root.join(destination);
    // Check existing ancestors before creating or canonicalizing any directory.
    let mut ancestor = output.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().context("restore destination parent")?;
    }
    ensure!(
        ancestor.canonicalize()?.starts_with(root),
        "restore destination leaves workspace"
    );
    fs::create_dir_all(&output)?;
    let output = output.canonicalize()?;
    ensure!(
        output.starts_with(root),
        "restore destination leaves workspace"
    );
    let store = Store {
        directory: Some(
            root.join(
                store
                    .directory
                    .as_deref()
                    .unwrap_or(Path::new(".flux/cache")),
            ),
        ),
        s3: store.s3.clone(),
    };
    restore(&output, &store, keys, paths, required)
}

#[derive(Serialize, Deserialize)]
struct SourceState {
    digest: String,
    seconds: u64,
    nanos: u32,
}

pub fn sources(root: &Path, target: &Path, restore: bool, include: &[PathBuf]) -> Result<()> {
    relative(target)?;
    let state_file = root.join(target).join(".flux-sources-v1.json");
    let mut ancestor = root.join(target);
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .context("source snapshot parent")?
            .to_owned();
    }
    ensure!(
        ancestor.canonicalize()?.starts_with(root),
        "source snapshot leaves workspace"
    );
    ensure!(
        !state_file
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink()),
        "source snapshot must not be a symlink"
    );
    if restore {
        if !state_file.is_file() {
            println!("no cached source timestamps");
            return Ok(());
        }
        let state: BTreeMap<PathBuf, SourceState> = serde_json::from_slice(&fs::read(state_file)?)?;
        let mut unchanged = 0;
        let mut changed = 0;
        for (path, saved) in state {
            relative(&path)?;
            let absolute = root.join(&path);
            if !absolute.is_file() || absolute.symlink_metadata()?.file_type().is_symlink() {
                continue;
            }
            ensure!(
                absolute.canonicalize()?.starts_with(root),
                "cached source leaves workspace"
            );
            let modified = if digest(&absolute)? == saved.digest {
                unchanged += 1;
                std::time::UNIX_EPOCH + std::time::Duration::new(saved.seconds, saved.nanos)
            } else {
                changed += 1;
                std::time::SystemTime::now()
            };
            fs::File::open(absolute)?.set_times(fs::FileTimes::new().set_modified(modified))?;
        }
        println!("source timestamps: {unchanged} unchanged, {changed} changed");
    } else {
        let listed = std::process::Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(root)
            .output()?;
        ensure!(listed.status.success(), "could not list source files");
        let listed = String::from_utf8(listed.stdout)?;
        let mut paths = listed
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect::<BTreeSet<_>>();
        paths.extend(include.iter().cloned());
        let mut state = BTreeMap::new();
        for path in paths {
            relative(&path)?;
            let absolute = root.join(&path);
            if !absolute.is_file() || absolute.symlink_metadata()?.file_type().is_symlink() {
                continue;
            }
            ensure!(
                absolute.canonicalize()?.starts_with(root),
                "source leaves workspace"
            );
            let modified = absolute
                .metadata()?
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)?;
            state.insert(
                path,
                SourceState {
                    digest: digest(&absolute)?,
                    seconds: modified.as_secs(),
                    nanos: modified.subsec_nanos(),
                },
            );
        }
        fs::create_dir_all(state_file.parent().context("source snapshot parent")?)?;
        fs::write(state_file, serde_json::to_vec(&state)?)?;
        println!("recorded {} source timestamps", state.len());
    }
    Ok(())
}
