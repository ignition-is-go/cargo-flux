use super::*;

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_root: PathBuf,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: PathBuf,
    targets: Vec<Target>,
    features: BTreeMap<String, serde_json::Value>,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Target {
    src_path: PathBuf,
}

#[derive(Deserialize)]
pub(super) struct Lock {
    pub(super) package: Vec<LockPackage>,
}

#[derive(Deserialize)]
pub(super) struct LockPackage {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) source: Option<String>,
}

struct Owner {
    name: String,
    features: BTreeSet<String>,
}

pub(super) struct Sources {
    pub(super) roots: BTreeSet<PathBuf>,
    targets: BTreeMap<PathBuf, Vec<Owner>>,
    package_roots: BTreeMap<String, BTreeSet<PathBuf>>,
    manifests: BTreeSet<PathBuf>,
    registry: BTreeSet<(String, String)>,
    git: BTreeSet<(String, String)>,
    cargo_home: Option<PathBuf>,
}

pub(super) fn manifests(paths: &[PathBuf]) -> Vec<PathBuf> {
    if paths.is_empty() {
        vec!["Cargo.toml".into()]
    } else {
        paths.to_vec()
    }
}

impl Sources {
    pub(super) fn load(root: &Path, paths: &[PathBuf]) -> Result<Self> {
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                #[cfg(windows)]
                let home = std::env::var_os("USERPROFILE");
                #[cfg(not(windows))]
                let home = std::env::var_os("HOME");
                home.map(|path| PathBuf::from(path).join(".cargo"))
            })
            .map(|path| {
                let path = root.join(path);
                path.canonicalize().unwrap_or(path)
            });
        let mut sources = Self {
            roots: BTreeSet::new(),
            targets: BTreeMap::new(),
            package_roots: BTreeMap::new(),
            manifests: BTreeSet::new(),
            registry: BTreeSet::new(),
            git: BTreeSet::new(),
            cargo_home,
        };
        let mut pending = manifests(paths)
            .into_iter()
            .map(|path| (root.join(path), None))
            .collect::<Vec<_>>();
        let mut visited = BTreeSet::new();
        while let Some((manifest, consuming_root)) = pending.pop() {
            let manifest = dunce::canonicalize(manifest)?;
            if !visited.insert((manifest.clone(), consuming_root.clone())) {
                continue;
            }
            let output = Command::new("cargo")
                .current_dir(root)
                .args([
                    "metadata",
                    "--no-deps",
                    "--format-version",
                    "1",
                    "--locked",
                    "--offline",
                    "--manifest-path",
                ])
                .arg(&manifest)
                .output()?;
            ensure!(
                output.status.success(),
                "Cargo metadata failed before compiler snapshot: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
            let declared = consuming_root.is_none();
            if declared {
                let lock = metadata.workspace_root.join("Cargo.lock");
                let lock: Lock = match fs::read_to_string(lock) {
                    Ok(text) => toml::from_str(&text)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Lock {
                        package: Vec::new(),
                    },
                    Err(error) => return Err(error.into()),
                };
                for package in lock.package {
                    if let Some(source) = package.source {
                        if source.starts_with("registry+") || source.starts_with("sparse+") {
                            sources.registry.insert((package.name, package.version));
                        } else if source.starts_with("git+")
                            && let Some((_, revision)) = source.rsplit_once('#')
                        {
                            sources.git.insert((package.name, revision.to_owned()));
                        }
                    }
                }
            }
            let consuming_root = consuming_root.unwrap_or(metadata.workspace_root.clone());
            sources.roots.insert(consuming_root.clone());
            for package in metadata.packages {
                if !declared && dunce::canonicalize(&package.manifest_path)? != manifest {
                    continue;
                }
                pending.extend(
                    package
                        .dependencies
                        .into_iter()
                        .filter_map(|dependency| dependency.path)
                        .map(|path| (path.join("Cargo.toml"), Some(consuming_root.clone()))),
                );
                sources
                    .package_roots
                    .entry(package.name.clone())
                    .or_default()
                    .insert(consuming_root.clone());
                sources.manifests.insert(
                    package
                        .manifest_path
                        .canonicalize()
                        .unwrap_or(package.manifest_path),
                );
                let features = package.features.into_keys().collect::<BTreeSet<_>>();
                for target in package.targets {
                    let path = target.src_path.canonicalize().unwrap_or(target.src_path);
                    sources.targets.entry(path).or_default().push(Owner {
                        name: package.name.clone(),
                        features: features.clone(),
                    });
                }
            }
        }
        Ok(sources)
    }

    pub(super) fn roots_for(&self, package: Option<&str>) -> &BTreeSet<PathBuf> {
        package
            .and_then(|name| self.package_roots.get(name))
            .unwrap_or(&self.roots)
    }

    /// Unknown source ownership is retained, never treated as proof of obsolescence.
    pub(super) fn current(
        &self,
        source: &Path,
        package: &str,
        features: &BTreeSet<String>,
    ) -> Result<Option<bool>> {
        if let Some(owners) = self.targets.get(source) {
            let owners = owners
                .iter()
                .filter(|owner| owner.name == package)
                .collect::<Vec<_>>();
            if !owners.is_empty() {
                return Ok(Some(
                    owners
                        .iter()
                        .any(|owner| features.is_subset(&owner.features)),
                ));
            }
        }
        if let Some(home) = &self.cargo_home {
            if let Ok(path) = source.strip_prefix(home.join("registry/src")) {
                let mut components = path.components();
                let _registry = components.next();
                if let Some(directory) =
                    components.next().and_then(|part| part.as_os_str().to_str())
                    && let Some(version) = directory.strip_prefix(&format!("{package}-"))
                {
                    return Ok(Some(
                        self.registry
                            .contains(&(package.to_owned(), version.to_owned())),
                    ));
                }
            }
            if let Ok(path) = source.strip_prefix(home.join("git/checkouts")) {
                let mut components = path.components();
                let _repository = components.next();
                if let Some(revision) = components.next().and_then(|part| part.as_os_str().to_str())
                    && revision.len() >= 7
                    && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Ok(Some(self.git.iter().any(|(name, full)| {
                        name == package && full.starts_with(revision)
                    })));
                }
            }
        }
        for directory in source.ancestors().skip(1) {
            let manifest = directory.join("Cargo.toml");
            if self.manifests.contains(&manifest) {
                return Ok(Some(false));
            }
            if manifest.is_file() {
                return Ok(None);
            }
        }
        Ok(None)
    }
}
