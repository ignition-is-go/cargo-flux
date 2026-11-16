//! Select reusable Cargo outputs using package sources and fingerprint dependencies.
use super::*;
use std::process::Command;

mod sources;

#[derive(Default)]
struct Unit {
    package: Option<String>,
    files: BTreeSet<PathBuf>,
    values: BTreeSet<u64>,
    dependencies: BTreeSet<u64>,
    sources: BTreeSet<PathBuf>,
    dep_info: Vec<String>,
    features: BTreeSet<String>,
    fingerprint: bool,
}

#[derive(Deserialize)]
struct Fingerprint {
    deps: Vec<(u64, String, bool, u64)>,
    features: Option<String>,
}

fn artifact_hash(name: &str) -> Option<&str> {
    let (_, hash) = name.split('.').next()?.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(hash)
}

fn fingerprint_value(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.len() != 16 {
        return None;
    }
    u64::from_str_radix(value, 16).ok().map(u64::swap_bytes)
}

fn source_path(workspace: &Path, dep_info: &str) -> Option<PathBuf> {
    let (_, dependencies) = dep_info.lines().next()?.split_once(": ")?;
    let mut source = String::new();
    let mut characters = dependencies.trim_start().chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.next_if_eq(&' ').is_some() => source.push(' '),
            ' ' => break,
            _ => source.push(character),
        }
    }
    if source.is_empty() {
        return None;
    }
    let path = workspace.join(source);
    Some(path.canonicalize().unwrap_or(path))
}

/// Identify one Cargo unit in a declared profile, including nested debug bundles.
fn unit_id(path: &Path, profiles: &BTreeSet<PathBuf>) -> Option<(PathBuf, String)> {
    for profile in profiles {
        let relative = path.strip_prefix(profile).ok();
        let Some(mut components) = relative.map(Path::components) else {
            continue;
        };
        let category = components.next()?.as_os_str();
        if ![".fingerprint", "deps", "examples", "build"]
            .iter()
            .any(|name| category == *name)
        {
            continue;
        }
        let name = components.next()?.as_os_str().to_str()?;
        if let Some(hash) = artifact_hash(name) {
            return Some((profile.clone(), hash.to_owned()));
        }
    }
    None
}

pub(super) fn select(
    root: &Path,
    files: &mut BTreeSet<PathBuf>,
    manifests: &[PathBuf],
) -> Result<Vec<fs::File>> {
    let sources = sources::Sources::load(root, manifests)?;
    let mut profiles = BTreeSet::new();
    for path in files.iter() {
        for ancestor in path.ancestors() {
            if ancestor
                .file_name()
                .is_some_and(|name| name == ".fingerprint")
            {
                profiles.insert(ancestor.parent().context("Cargo profile")?.to_owned());
            }
        }
    }
    let mut locks = Vec::new();
    for profile in &profiles {
        for name in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
            let path = root.join(profile).join(name);
            match fs::OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => {
                    file.try_lock()
                        .with_context(|| format!("Cargo profile is busy: {}", profile.display()))?;
                    locks.push(file);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut units = BTreeMap::<(PathBuf, String), Unit>::new();
    for path in files.iter() {
        let Some(id) = unit_id(path, &profiles) else {
            continue;
        };
        let unit = units.entry(id).or_default();
        unit.files.insert(path.clone());
        if path
            .components()
            .any(|component| component.as_os_str() == ".fingerprint")
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            let fingerprint: Fingerprint =
                serde_json::from_reader(fs::File::open(root.join(path))?)
                    .with_context(|| format!("invalid Cargo fingerprint: {}", path.display()))?;
            unit.fingerprint = true;
            unit.package = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .and_then(|name| name.rsplit_once('-'))
                .map(|(package, _)| package.to_owned());
            unit.dependencies
                .extend(fingerprint.deps.into_iter().map(|dependency| dependency.3));
            if let Some(features) = fingerprint.features.filter(|features| !features.is_empty()) {
                unit.features
                    .extend(serde_json::from_str::<Vec<String>>(&features)?);
            }
            let value = fs::read_to_string(root.join(path.with_extension("")))?;
            if let Some(value) = fingerprint_value(&value) {
                unit.values.insert(value);
            }
        } else if path.extension().is_some_and(|extension| extension == "d") {
            unit.dep_info.push(fs::read_to_string(root.join(path))?);
        }
    }
    for unit in units.values_mut() {
        for dep_info in &unit.dep_info {
            for workspace in sources.roots_for(unit.package.as_deref()) {
                if let Some(source) = source_path(workspace, dep_info) {
                    unit.sources.insert(source);
                }
            }
        }
    }
    let mut obsolete = BTreeSet::new();
    let mut protected = BTreeSet::new();
    let mut protected_values = BTreeSet::new();
    for (id, unit) in &units {
        let statuses = unit
            .sources
            .iter()
            .map(|source| match &unit.package {
                Some(package) => sources.current(source, package, &unit.features),
                None => Ok(None),
            })
            .collect::<Result<Vec<_>>>()?;
        if !unit.fingerprint
            || (!statuses.is_empty() && statuses.iter().all(|status| *status == Some(false)))
        {
            obsolete.insert(id.clone());
        }
        if unit.fingerprint && statuses.iter().any(Option::is_none) {
            protected.insert(id.clone());
            protected_values.extend(unit.values.iter().chain(&unit.dependencies).copied());
        }
    }
    loop {
        let before = protected_values.len();
        for (id, unit) in &units {
            if !unit.values.is_disjoint(&protected_values) {
                protected.insert(id.clone());
                protected_values.extend(unit.dependencies.iter().copied());
            }
        }
        if protected_values.len() == before {
            break;
        }
    }
    obsolete.retain(|id| !protected.contains(id));
    loop {
        let live_values = units
            .iter()
            .filter(|(id, _)| !obsolete.contains(*id))
            .flat_map(|(_, unit)| unit.values.iter().copied())
            .collect::<BTreeSet<_>>();
        let dead_values = units
            .iter()
            .filter(|(id, _)| obsolete.contains(*id))
            .flat_map(|(_, unit)| unit.values.iter().copied())
            .filter(|value| !live_values.contains(value))
            .collect::<BTreeSet<_>>();
        let newly_obsolete = units
            .iter()
            .filter_map(|(id, unit)| {
                (!obsolete.contains(id) && !unit.dependencies.is_disjoint(&dead_values))
                    .then_some(id.clone())
            })
            .collect::<Vec<_>>();
        if newly_obsolete.is_empty() {
            break;
        }
        obsolete.extend(newly_obsolete);
    }
    let excluded = obsolete
        .iter()
        .flat_map(|id| units[id].files.iter().cloned())
        .collect::<BTreeSet<_>>();
    let mut obsolete_ids = excluded
        .iter()
        .map(|path| file_id::get_file_id(root.join(path)))
        .collect::<std::io::Result<std::collections::HashSet<_>>>()?;
    for (id, unit) in &units {
        if !obsolete.contains(id) {
            for path in &unit.files {
                obsolete_ids.remove(&file_id::get_file_id(root.join(path))?);
            }
        }
    }
    let aliases = files
        .iter()
        .map(|path| {
            file_id::get_file_id(root.join(path))
                .map(|id| (path.clone(), obsolete_ids.contains(&id)))
        })
        .collect::<std::io::Result<BTreeMap<_, _>>>()?;
    let before = files.len();
    files.retain(|path| {
        !excluded.contains(path)
            && !aliases[path]
            && !profiles
                .iter()
                .any(|profile| path.starts_with(profile.join("incremental")))
    });
    eprintln!(
        "storage compiler snapshot: excluded {} files from {} obsolete units and incremental sessions",
        before - files.len(),
        obsolete.len()
    );
    Ok(locks)
}

/// Local package versions change Cargo unit identities, even with identical source paths.
pub(super) fn local_packages(
    root: &Path,
    manifests: &[PathBuf],
) -> Result<BTreeMap<PathBuf, Vec<(String, String)>>> {
    let mut identities = BTreeMap::new();
    for manifest in sources::manifests(manifests) {
        let path = manifest
            .parent()
            .context("Cargo manifest parent")?
            .join("Cargo.lock");
        let text = match fs::read_to_string(root.join(&path)) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                identities.insert(path, Vec::new());
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let lock: sources::Lock = toml::from_str(&text)?;
        let mut packages = lock
            .package
            .into_iter()
            .filter(|package| package.source.is_none())
            .map(|package| (package.name, package.version))
            .collect::<Vec<_>>();
        packages.sort();
        identities.insert(path, packages);
    }
    Ok(identities)
}

pub(super) fn configuration(
    root: &Path,
    manifests: &[PathBuf],
) -> Result<Option<sources::Identity>> {
    if manifests.is_empty() && !root.join("Cargo.toml").exists() {
        return Ok(None);
    }
    Ok(Some(sources::Sources::load(root, manifests)?.identity))
}
