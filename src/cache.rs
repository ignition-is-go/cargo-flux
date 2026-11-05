//! Opt-in, content-addressed root-task outputs. Cargo target snapshots are a
//! different cache: restoring compiler artifacts must not skip validation.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::tasks::TaskCommand;

mod s3;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Store {
    pub directory: Option<PathBuf>,
    pub s3: Option<S3>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct S3 {
    pub bucket: Option<String>,
    pub bucket_env: Option<String>,
    pub endpoint: Option<String>,
    pub endpoint_env: Option<String>,
    #[serde(default = "default_prefix")]
    pub prefix: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_prefix() -> String {
    "cargo-flux/v1".into()
}
fn default_timeout() -> u64 {
    120
}

fn configured_value(
    literal: &Option<String>,
    variable: &Option<String>,
    label: &str,
) -> Result<Option<String>> {
    ensure!(
        literal.is_none() || variable.is_none(),
        "configure {label} or {label}_env, not both"
    );
    match variable {
        Some(name) => Ok(Some(
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .with_context(|| {
                    format!("cache {label} environment variable `{name}` is missing or empty")
                })?,
        )),
        None => Ok(literal.clone()),
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskCache {
    pub inputs: Vec<String>,
    pub optional_inputs: Vec<String>,
    pub paths: Vec<PathBuf>,
    pub env: Vec<String>,
    pub tools: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCache {
    pub store: Store,
    pub task: TaskCache,
    pub required: bool,
    pub hit_required: bool,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    key: String,
    stdout: Option<String>,
    files: BTreeMap<PathBuf, String>,
}

const MANIFEST: &str = "__flux_cache_manifest.json";

pub struct Session<'a> {
    root: &'a Path,
    config: &'a ResolvedCache,
    key: String,
    name: String,
    commands: Vec<TaskCommand>,
    dependency_outputs: BTreeMap<String, BTreeMap<String, String>>,
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn relative(path: &Path) -> Result<()> {
    ensure!(!path.as_os_str().is_empty(), "empty cache path");
    ensure!(
        path.components().all(|c| matches!(c, Component::Normal(_))),
        "cache paths must be relative without traversal: {}",
        path.display()
    );
    ensure!(
        !path.starts_with(".git") && path != Path::new(MANIFEST),
        "reserved cache path"
    );
    Ok(())
}

fn files(root: &Path, path: &Path, result: &mut BTreeSet<PathBuf>) -> Result<()> {
    relative(path)?;
    let full = root.join(path);
    ensure!(
        full.canonicalize()?.starts_with(root),
        "cache path leaves workspace"
    );
    let metadata = fs::symlink_metadata(&full)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "cache symlinks are not supported"
    );
    if metadata.is_dir() {
        for child in fs::read_dir(full)? {
            files(root, &path.join(child?.file_name()), result)?;
        }
    } else {
        ensure!(metadata.is_file(), "cache paths must contain regular files");
        result.insert(path.to_owned());
    }
    Ok(())
}

fn digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

impl<'a> Session<'a> {
    pub fn prepare(
        root: &'a Path,
        name: &str,
        config: &'a ResolvedCache,
        commands: &[TaskCommand],
        dependency_outputs: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<Self> {
        ensure!(
            !config.task.inputs.is_empty(),
            "cached tasks must declare inputs"
        );
        for path in &config.task.paths {
            relative(path)?;
            ensure!(
                !config
                    .task
                    .paths
                    .iter()
                    .any(|other| other != path && path.starts_with(other)),
                "cache output paths must not overlap"
            );
        }
        let mut hash = Sha256::new();
        field(&mut hash, b"cargo-flux-root-cache-v1");
        field(&mut hash, root.as_os_str().as_encoded_bytes());
        field(&mut hash, std::env::consts::OS.as_bytes());
        field(&mut hash, std::env::consts::ARCH.as_bytes());
        field(&mut hash, name.as_bytes());
        field(
            &mut hash,
            &serde_json::to_vec(&(commands, &config.task, dependency_outputs))?,
        );
        for name in &config.task.env {
            field(&mut hash, name.as_bytes());
            if name.contains(['*', '?', '[']) {
                let pattern = glob::Pattern::new(name)?;
                let values: BTreeMap<_, _> = std::env::vars_os()
                    .filter(|(key, _)| key.to_str().is_some_and(|key| pattern.matches(key)))
                    .collect();
                field(&mut hash, b"environment-glob");
                field(&mut hash, &(values.len() as u64).to_le_bytes());
                for (key, value) in values {
                    field(&mut hash, key.as_encoded_bytes());
                    field(&mut hash, value.as_encoded_bytes());
                }
                continue;
            }
            match std::env::var_os(name) {
                Some(value) => {
                    field(&mut hash, b"present");
                    field(&mut hash, value.as_encoded_bytes());
                }
                None => field(&mut hash, b"absent"),
            }
        }
        for tool in &config.task.tools {
            let (program, args) = tool.split_first().context("empty cache tool probe")?;
            let output = Command::new(program)
                .args(args)
                .current_dir(root)
                .stdin(Stdio::null())
                .output()?;
            ensure!(output.status.success(), "cache tool probe failed");
            field(&mut hash, &output.stdout);
            field(&mut hash, &output.stderr);
        }
        let mut inputs = BTreeSet::new();
        for (pattern, required) in config
            .task
            .inputs
            .iter()
            .map(|p| (p, true))
            .chain(config.task.optional_inputs.iter().map(|p| (p, false)))
        {
            relative(Path::new(pattern))?;
            field(&mut hash, pattern.as_bytes());
            let mut matched = false;
            for path in glob::glob(root.join(pattern).to_str().context("non-UTF8 cache glob")?)? {
                let path = path?;
                matched = true;
                files(root, path.strip_prefix(root)?, &mut inputs)?;
            }
            ensure!(
                matched || !required,
                "cache input pattern matched nothing: {pattern}"
            );
        }
        for path in inputs {
            field(&mut hash, path.as_os_str().as_encoded_bytes());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                field(
                    &mut hash,
                    &fs::metadata(root.join(&path))?
                        .permissions()
                        .mode()
                        .to_le_bytes(),
                );
            }
            field(&mut hash, digest(&root.join(path))?.as_bytes());
        }
        Ok(Self {
            root,
            config,
            key: format!("{:x}", hash.finalize()),
            name: name.to_owned(),
            commands: commands.to_vec(),
            dependency_outputs: dependency_outputs.clone(),
        })
    }

    /// Validate the complete archive in isolation before modifying any output.
    pub fn restore(&self) -> Result<Option<Option<String>>> {
        let temporary = tempfile::tempdir()?;
        let archive = temporary.path().join("cache.tar");
        if !self.download(&archive)? {
            return Ok(None);
        }
        let stage = temporary.path().join("outputs");
        fs::create_dir(&stage)?;
        let stage = stage.canonicalize()?;
        let mut archive = tar::Archive::new(fs::File::open(archive)?);
        let mut entries = archive.entries()?;
        let mut first = entries.next().context("empty cache archive")??;
        ensure!(
            first.path()?.as_ref() == Path::new(MANIFEST),
            "missing cache manifest"
        );
        ensure!(first.size() <= 16 * 1024 * 1024, "oversized cache manifest");
        let manifest: Manifest = serde_json::from_reader(&mut first)?;
        ensure!(manifest.key == self.key, "cache key mismatch");
        for entry in entries {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            relative(&path)?;
            ensure!(
                self.config
                    .task
                    .paths
                    .iter()
                    .any(|output| path.starts_with(output)),
                "undeclared cache output"
            );
            ensure!(
                entry.header().entry_type().is_file() || entry.header().entry_type().is_dir(),
                "cache links and special files are forbidden"
            );
            ensure!(entry.unpack_in(&stage)?, "invalid cache entry");
        }
        let mut actual = BTreeSet::new();
        for path in &self.config.task.paths {
            files(&stage, path, &mut actual)?;
        }
        ensure!(
            actual == manifest.files.keys().cloned().collect(),
            "cache file list mismatch"
        );
        for (path, expected) in &manifest.files {
            ensure!(
                digest(&stage.join(path))? == *expected,
                "cache content checksum mismatch"
            );
        }
        for path in &self.config.task.paths {
            let output = self.root.join(path);
            let parent = output.parent().context("output parent")?;
            // Existing ancestors must not redirect an extraction outside root.
            let mut ancestor = parent;
            while !ancestor.exists() {
                ancestor = ancestor.parent().context("output ancestor")?;
            }
            ensure!(
                ancestor.canonicalize()?.starts_with(self.root),
                "cache output parent leaves workspace"
            );
            fs::create_dir_all(parent)?;
            let existing = if output.symlink_metadata().is_ok() {
                let meta = fs::symlink_metadata(&output)?;
                ensure!(
                    !meta.file_type().is_symlink(),
                    "refusing to replace a symlink output"
                );
                Some(meta)
            } else {
                None
            };
            // Copy through an adjacent temporary path: temp and workspace may
            // be on different filesystems, and incomplete files are never exposed.
            if stage.join(path).is_dir() {
                let temporary = tempfile::tempdir_in(parent)?;
                let complete = temporary.path().join("complete");
                copy_directory(&stage.join(path), &complete)?;
                if let Some(meta) = existing {
                    if meta.is_dir() {
                        fs::remove_dir_all(&output)?;
                    } else {
                        fs::remove_file(&output)?;
                    }
                }
                fs::rename(complete, output)?;
            } else {
                let mut temp = tempfile::NamedTempFile::new_in(parent)?;
                std::io::copy(&mut fs::File::open(stage.join(path))?, &mut temp)?;
                temp.as_file()
                    .set_permissions(fs::metadata(stage.join(path))?.permissions())?;
                if existing.is_some_and(|meta| meta.is_dir()) {
                    fs::remove_dir_all(&output)?;
                }
                temp.persist(&output)?;
            }
        }
        Ok(Some(manifest.stdout))
    }

    pub fn save(&self, stdout: Option<String>) -> Result<()> {
        let current = Self::prepare(
            self.root,
            &self.name,
            self.config,
            &self.commands,
            &self.dependency_outputs,
        )?;
        ensure!(
            current.key == self.key,
            "task inputs changed during execution; snapshot not saved"
        );
        let mut paths = BTreeSet::new();
        for path in &self.config.task.paths {
            files(self.root, path, &mut paths)?;
        }
        let hashes = paths
            .into_iter()
            .map(|path| Ok((path.clone(), digest(&self.root.join(path))?)))
            .collect::<Result<_>>()?;
        let manifest = serde_json::to_vec(&Manifest {
            key: self.key.clone(),
            stdout,
            files: hashes,
        })?;
        let mut archive = tempfile::NamedTempFile::new()?;
        {
            let mut tar = tar::Builder::new(&mut archive);
            let mut header = tar::Header::new_gnu();
            header.set_size(manifest.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            tar.append_data(&mut header, MANIFEST, &manifest[..])?;
            for path in &self.config.task.paths {
                if self.root.join(path).is_dir() {
                    tar.append_dir_all(path, self.root.join(path))?;
                } else {
                    tar.append_path_with_name(self.root.join(path), path)?;
                }
            }
            tar.finish()?;
        }
        archive.flush()?;
        if self.config.store.s3.is_some() {
            self.s3("upload", archive.path())?;
        } else {
            let destination = self.local_path();
            let parent = destination.parent().context("cache directory")?;
            fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            std::io::copy(&mut fs::File::open(archive.path())?, &mut file)?;
            file.persist(destination)?;
        }
        Ok(())
    }

    fn local_path(&self) -> PathBuf {
        self.root
            .join(
                self.config
                    .store
                    .directory
                    .as_deref()
                    .unwrap_or(Path::new(".flux/cache")),
            )
            .join(format!("{}.tar", self.key))
    }

    fn download(&self, destination: &Path) -> Result<bool> {
        if self.config.store.s3.is_some() {
            return self.s3("download", destination);
        }
        match fs::copy(self.local_path(), destination) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn s3(&self, operation: &str, path: &Path) -> Result<bool> {
        s3::transfer(
            self.config.store.s3.as_ref().context("missing S3 store")?,
            &self.key,
            operation,
            path,
        )
    }
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let output = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&entry.path(), &output)?;
        } else {
            fs::copy(entry.path(), output)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_archive_link_cannot_modify_existing_outputs() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        fs::write(root.join("source"), "input").unwrap();
        fs::create_dir(root.join("dist")).unwrap();
        fs::write(root.join("dist/result"), "original").unwrap();
        let config = ResolvedCache {
            required: false,
            hit_required: false,
            store: Store::default(),
            task: TaskCache {
                inputs: vec!["source".into()],
                paths: vec!["dist".into()],
                ..Default::default()
            },
        };
        let session = Session::prepare(&root, "produce", &config, &[], &BTreeMap::new()).unwrap();
        fs::create_dir_all(session.local_path().parent().unwrap()).unwrap();
        let mut archive = tar::Builder::new(fs::File::create(session.local_path()).unwrap());
        let manifest = serde_json::to_vec(&Manifest {
            key: session.key.clone(),
            stdout: None,
            files: BTreeMap::new(),
        })
        .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(manifest.len() as u64);
        header.set_cksum();
        archive
            .append_data(&mut header, MANIFEST, &manifest[..])
            .unwrap();
        let mut regular = tar::Header::new_gnu();
        regular.set_mode(0o600);
        regular.set_size(7);
        regular.set_cksum();
        archive
            .append_data(&mut regular, "dist/result", &b"changed"[..])
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_mode(0o777);
        link.set_size(0);
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_link_name("../../outside").unwrap();
        link.set_cksum();
        archive
            .append_data(&mut link, "dist/link", std::io::empty())
            .unwrap();
        archive.finish().unwrap();
        assert!(session.restore().is_err());
        assert_eq!(
            fs::read_to_string(root.join("dist/result")).unwrap(),
            "original"
        );
        assert!(!root.join("dist/link").exists());
    }
}
