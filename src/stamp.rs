//! Version stamping for workspace manifests

use crate::manifest::{Ecosystem, Package};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StampConfig {
    pub exclude_versions: Vec<String>,
}

impl StampConfig {
    pub fn load(root: &Path) -> Result<Self> {
        #[derive(Default, Deserialize)]
        struct FluxStampConfig {
            #[serde(default)]
            stamp: StampConfig,
            #[serde(flatten)]
            _other: std::collections::BTreeMap<String, toml::Value>,
        }

        let path = root.join("flux.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read stamp config {}", path.display()))?;
        let config: FluxStampConfig = toml::from_str(&content)
            .with_context(|| format!("failed to parse stamp config {}", path.display()))?;
        Ok(config.stamp)
    }
}

#[derive(Debug, Default)]
pub struct StampOptions {
    pub packages: Vec<String>,
    pub exclude: Vec<String>,
    pub exclude_versions: Vec<String>,
}

/// Stamp every discovered Cargo and JavaScript package.
#[cfg(test)]
pub fn stamp_all(root: &Path, packages: &[Package], version: &str) -> Result<Vec<String>> {
    stamp_selected(root, packages, version, &StampOptions::default())
}

/// Stamp packages selected by name and current-version policy.
pub fn stamp_selected(
    root: &Path,
    packages: &[Package],
    version: &str,
    options: &StampOptions,
) -> Result<Vec<String>> {
    validate_selectors(packages, &options.packages, "package")?;
    validate_selectors(packages, &options.exclude, "exclude")?;

    let includes: HashSet<&str> = options.packages.iter().map(String::as_str).collect();
    let excludes: HashSet<&str> = options.exclude.iter().map(String::as_str).collect();
    let excluded_versions: HashSet<&str> = options
        .exclude_versions
        .iter()
        .map(String::as_str)
        .collect();
    let workspace_version = cargo_workspace_version(&root.join("Cargo.toml"))?;

    let mut selected = Vec::new();
    for package in packages {
        if (!includes.is_empty() && !includes.contains(package.name.as_str()))
            || excludes.contains(package.name.as_str())
        {
            continue;
        }
        let current = package_version(package, workspace_version.as_deref())?;
        if current
            .as_deref()
            .is_some_and(|value| excluded_versions.contains(value))
        {
            continue;
        }
        if package.ecosystem != Ecosystem::Uv {
            selected.push(package);
        }
    }

    let selected_cargo_paths: HashSet<PathBuf> = selected
        .iter()
        .filter(|p| p.ecosystem == Ecosystem::Cargo)
        .filter_map(|p| normalized_existing_path(&p.manifest_path))
        .collect();
    let selected_js_names: HashSet<&str> = selected
        .iter()
        .filter(|p| p.ecosystem == Ecosystem::Js)
        .map(|p| p.name.as_str())
        .collect();

    let filtered = !options.packages.is_empty()
        || !options.exclude.is_empty()
        || !options.exclude_versions.is_empty();
    let mut modified = Vec::new();
    let mut stamped_paths = HashSet::new();
    let root_cargo = root.join("Cargo.toml");
    let root_package_selected = selected.iter().any(|p| {
        p.ecosystem == Ecosystem::Cargo && same_existing_path(&p.manifest_path, &root_cargo)
    });
    let cargo_selected = selected.iter().any(|p| p.ecosystem == Ecosystem::Cargo);
    let stamp_workspace = selected
        .iter()
        .filter(|p| p.ecosystem == Ecosystem::Cargo)
        .try_fold(false, |found, package| {
            Ok::<_, anyhow::Error>(
                found || cargo_package_inherits_workspace(&package.manifest_path)?,
            )
        })?
        || (!filtered && packages.iter().all(|p| p.ecosystem != Ecosystem::Cargo));

    if root_cargo.exists()
        && (root_package_selected || cargo_selected || stamp_workspace)
        && stamp_cargo_toml_filtered(
            &root_cargo,
            version,
            root_package_selected,
            stamp_workspace,
            &selected_cargo_paths,
        )?
    {
        modified.push(root_cargo.display().to_string());
        stamped_paths.insert(normalized_existing_path(&root_cargo).unwrap_or(root_cargo.clone()));
    }

    for package in selected {
        let normalized = normalized_existing_path(&package.manifest_path)
            .unwrap_or_else(|| package.manifest_path.clone());
        if stamped_paths.contains(&normalized) {
            continue;
        }
        let was_modified = match package.ecosystem {
            Ecosystem::Cargo => stamp_cargo_toml_filtered(
                &package.manifest_path,
                version,
                true,
                false,
                &selected_cargo_paths,
            )?,
            Ecosystem::Js => {
                stamp_package_json(&package.manifest_path, version, &selected_js_names)?
            }
            Ecosystem::Uv => false,
        };
        if was_modified {
            modified.push(package.manifest_path.display().to_string());
        }
    }

    Ok(modified)
}

fn validate_selectors(packages: &[Package], selectors: &[String], option: &str) -> Result<()> {
    for selector in selectors {
        if !packages.iter().any(|package| package.name == *selector) {
            bail!("--{option} selector `{selector}` did not match any workspace package");
        }
    }
    Ok(())
}

fn package_version(package: &Package, workspace_version: Option<&str>) -> Result<Option<String>> {
    let content = std::fs::read_to_string(&package.manifest_path)
        .with_context(|| format!("failed to read {}", package.manifest_path.display()))?;
    match package.ecosystem {
        Ecosystem::Cargo => {
            let value: toml::Value = toml::from_str(&content)
                .with_context(|| format!("failed to parse {}", package.manifest_path.display()))?;
            let version = value.get("package").and_then(|p| p.get("version"));
            Ok(match version {
                Some(toml::Value::String(value)) => Some(value.clone()),
                Some(toml::Value::Table(table))
                    if table.get("workspace").and_then(toml::Value::as_bool) == Some(true) =>
                {
                    workspace_version.map(str::to_owned)
                }
                _ => None,
            })
        }
        Ecosystem::Js => {
            let value: serde_json::Value = serde_json::from_str(&content)
                .with_context(|| format!("failed to parse {}", package.manifest_path.display()))?;
            Ok(value
                .get("version")
                .and_then(|v| v.as_str())
                .map(str::to_owned))
        }
        Ecosystem::Uv => Ok(None),
    }
}

fn cargo_workspace_version(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(value
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned))
}

fn cargo_package_inherits_workspace(path: &Path) -> Result<bool> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(value
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_table)
        .and_then(|v| v.get("workspace"))
        .and_then(toml::Value::as_bool)
        == Some(true))
}

fn normalized_existing_path(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}

fn same_existing_path(left: &Path, right: &Path) -> bool {
    match (
        normalized_existing_path(left),
        normalized_existing_path(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

/// Stamp version into a Cargo.toml file, preserving formatting.
#[cfg(test)]
fn stamp_cargo_toml(path: &Path, version: &str) -> Result<bool> {
    stamp_cargo_toml_filtered(path, version, true, true, &HashSet::new())
}

fn stamp_cargo_toml_filtered(
    path: &Path,
    version: &str,
    stamp_package: bool,
    stamp_workspace: bool,
    selected_targets: &HashSet<PathBuf>,
) -> Result<bool> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let mut modified = false;

    if stamp_package
        && let Some(pkg) = doc.get_mut("package").and_then(|p| p.as_table_mut())
        && pkg.get("version").is_some_and(toml_edit::Item::is_value)
        && pkg["version"].as_str().is_some()
        && pkg["version"].as_str() != Some(version)
    {
        pkg["version"] = toml_edit::value(version);
        modified = true;
    }

    if stamp_workspace
        && let Some(ws_pkg) = doc
            .get_mut("workspace")
            .and_then(|w| w.as_table_mut())
            .and_then(|w| w.get_mut("package"))
            .and_then(|p| p.as_table_mut())
        && ws_pkg.contains_key("version")
        && ws_pkg["version"].as_str() != Some(version)
    {
        ws_pkg["version"] = toml_edit::value(version);
        modified = true;
    }

    let manifest_dir = path.parent().unwrap_or_else(|| Path::new("."));
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = doc.get_mut(section).and_then(|d| d.as_table_mut()) {
            stamp_path_dep_versions(deps, manifest_dir, selected_targets, version, &mut modified);
        }
    }
    if let Some(ws_deps) = doc
        .get_mut("workspace")
        .and_then(|w| w.as_table_mut())
        .and_then(|w| w.get_mut("dependencies"))
        .and_then(|d| d.as_table_mut())
    {
        stamp_path_dep_versions(
            ws_deps,
            manifest_dir,
            selected_targets,
            version,
            &mut modified,
        );
    }
    if let Some(targets) = doc
        .get_mut("target")
        .and_then(|target| target.as_table_mut())
    {
        for (_target_name, target) in targets.iter_mut() {
            let Some(target) = target.as_table_mut() else {
                continue;
            };
            for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(deps) = target.get_mut(section).and_then(|deps| deps.as_table_mut()) {
                    stamp_path_dep_versions(
                        deps,
                        manifest_dir,
                        selected_targets,
                        version,
                        &mut modified,
                    );
                }
            }
        }
    }

    if modified {
        std::fs::write(path, doc.to_string())
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(modified)
}

fn stamp_path_dep_versions(
    deps: &mut toml_edit::Table,
    manifest_dir: &Path,
    selected_targets: &HashSet<PathBuf>,
    version: &str,
    modified: &mut bool,
) {
    for (_key, dep) in deps.iter_mut() {
        let (dep_path, old_version) = if let Some(table) = dep.as_inline_table() {
            (
                table
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                table
                    .get("version")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            )
        } else if let Some(table) = dep.as_table() {
            (
                table
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                table
                    .get("version")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
            )
        } else {
            (None, None)
        };
        let (Some(dep_path), Some(old_version)) = (dep_path, old_version) else {
            continue;
        };
        let target_manifest = manifest_dir.join(dep_path).join("Cargo.toml");
        // An empty target set is the legacy unfiltered helper behavior used by
        // focused tests; normal stamping always supplies discovered targets.
        if !selected_targets.is_empty()
            && normalized_existing_path(&target_manifest)
                .is_none_or(|target| !selected_targets.contains(&target))
        {
            continue;
        }
        if old_version == version {
            continue;
        }
        if let Some(table) = dep.as_inline_table_mut() {
            table.insert("version", toml_edit::value(version).into_value().unwrap());
        } else if let Some(table) = dep.as_table_mut() {
            table["version"] = toml_edit::value(version);
        }
        *modified = true;
    }
}

/// Stamp version into a package.json file, updating workspace dependency versions.
/// Also stamps a sibling deno.json if one exists.
fn stamp_package_json(
    path: &Path,
    version: &str,
    workspace_package_names: &std::collections::HashSet<&str>,
) -> Result<bool> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut json: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let Some(obj) = json.as_object_mut() else {
        return Ok(false);
    };

    if !obj.contains_key("version") {
        return Ok(false);
    }

    obj.insert(
        "version".to_string(),
        serde_json::Value::String(version.to_string()),
    );

    // Update versioned workspace dependency references
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(deps) = obj.get_mut(section).and_then(|v| v.as_object_mut()) {
            stamp_workspace_dep_versions(deps, version, workspace_package_names);
        }
    }

    let output = serde_json::to_string_pretty(&json)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    std::fs::write(path, format!("{}\n", output))
        .with_context(|| format!("failed to write {}", path.display()))?;

    // Stamp sibling deno.json if it exists
    if let Some(dir) = path.parent() {
        let deno_path = dir.join("deno.json");
        if deno_path.exists() {
            stamp_deno_json(&deno_path, version, workspace_package_names)?;
        }
    }

    Ok(true)
}

fn stamp_workspace_dep_versions(
    deps: &mut serde_json::Map<String, serde_json::Value>,
    version: &str,
    workspace_package_names: &std::collections::HashSet<&str>,
) {
    for (name, value) in deps.iter_mut() {
        if !workspace_package_names.contains(name.as_str()) {
            continue;
        }
        if let Some(specifier) = value.as_str()
            && let Some(resolved) = resolve_dep_specifier(specifier, version)
        {
            *value = serde_json::Value::String(resolved);
        }
    }
}

/// Resolve a dependency specifier to a concrete version for publishing.
/// Handles both `workspace:` protocol and plain semver ranges.
/// Returns None for path-like specifiers (`file:`, `link:`, `portal:`).
fn resolve_dep_specifier(specifier: &str, version: &str) -> Option<String> {
    // Strip workspace: prefix if present, then resolve the inner specifier
    let inner = specifier.strip_prefix("workspace:").unwrap_or(specifier);
    match inner {
        "*" => Some(version.to_string()),
        "^" => Some(format!("^{version}")),
        "~" => Some(format!("~{version}")),
        s if s.starts_with("file:") || s.starts_with("link:") || s.starts_with("portal:") => None,
        _ => {
            // Extract the range operator prefix (^, ~, >=, etc.) before the version digits
            let version_start = inner.find(|c: char| c.is_ascii_digit())?;
            Some(format!("{}{}", &inner[..version_start], version))
        }
    }
}

/// Stamp version into a deno.json file and update JSR import specifier versions.
fn stamp_deno_json(
    path: &Path,
    version: &str,
    selected_package_names: &HashSet<&str>,
) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut json: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let Some(obj) = json.as_object_mut() else {
        return Ok(());
    };

    if obj.contains_key("version") {
        obj.insert(
            "version".to_string(),
            serde_json::Value::String(version.to_string()),
        );
    }

    // Update JSR import specifier versions (jsr:@scope/name@version → jsr:@scope/name@new_version)
    if let Some(imports) = obj.get_mut("imports").and_then(|v| v.as_object_mut()) {
        for value in imports.values_mut() {
            if let Some(specifier) = value.as_str()
                && let Some(package_name) = jsr_package_name(specifier)
                && selected_package_names.contains(package_name)
                && let Some(updated) = update_jsr_specifier(specifier, version)
            {
                *value = serde_json::Value::String(updated);
            }
        }
    }

    let output = serde_json::to_string_pretty(&json)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    std::fs::write(path, format!("{}\n", output))
        .with_context(|| format!("failed to write {}", path.display()))?;

    eprintln!("  stamped {}", path.display());
    Ok(())
}

fn jsr_package_name(specifier: &str) -> Option<&str> {
    let rest = specifier.strip_prefix("jsr:")?;
    let version_at = rest.rfind('@')?;
    (version_at > 0).then_some(&rest[..version_at])
}

/// Update a JSR specifier's version: `jsr:@scope/name@old` → `jsr:@scope/name@new`
fn update_jsr_specifier(specifier: &str, version: &str) -> Option<String> {
    let rest = specifier.strip_prefix("jsr:")?;
    let at_idx = rest.rfind('@')?;
    // Ensure we're not splitting at the scope @ (e.g. @myko/rs)
    if at_idx == 0 {
        return None;
    }
    Some(format!("jsr:{}@{}", &rest[..at_idx], version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Ecosystem, Package, PackageId};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn stamps_cargo_toml_package_version() {
        let root = temp_dir("stamp-cargo");
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "my-crate"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();

        let modified = stamp_cargo_toml(&root.join("Cargo.toml"), "2.0.0").unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(content.contains("version = \"2.0.0\""));
        assert!(content.contains("name = \"my-crate\""));
        assert!(content.contains("edition = \"2024\""));
    }

    #[test]
    fn stamps_cargo_toml_workspace_package_version() {
        let root = temp_dir("stamp-cargo-ws-pkg");
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();

        let modified = stamp_cargo_toml(&root.join("Cargo.toml"), "2.0.0").unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(content.contains("version = \"2.0.0\""));
        assert!(content.contains("edition = \"2024\""));
    }

    #[test]
    fn stamps_workspace_dependencies_path_dep_versions() {
        let root = temp_dir("stamp-ws-deps");
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/*"]

[workspace.package]
version = "0.1.0"

[workspace.dependencies]
serde = "1.0"
my-lib = { version = "0.1.0", path = "crates/my-lib" }
"#,
        )
        .unwrap();

        let modified = stamp_cargo_toml(&root.join("Cargo.toml"), "2.0.0").unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        // workspace.package version updated
        assert!(content.contains("[workspace.package]\nversion = \"2.0.0\""));
        // workspace.dependencies path dep updated
        assert!(content.contains("my-lib = { version = \"2.0.0\", path = \"crates/my-lib\" }"));
        // external dep unchanged
        assert!(content.contains("serde = \"1.0\""));
    }

    #[test]
    fn stamps_cargo_toml_workspace_dep_versions() {
        let root = temp_dir("stamp-cargo-deps");
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "app"
version = "0.1.0"

[dependencies]
shared = { version = "0.1.0", path = "../shared" }
serde = "1"
"#,
        )
        .unwrap();

        let modified = stamp_cargo_toml(&root.join("Cargo.toml"), "2.0.0").unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(content.contains("version = \"2.0.0\""));
        // External deps unchanged
        assert!(content.contains("serde = \"1\""));
    }

    #[test]
    fn stamps_package_json_version() {
        let root = temp_dir("stamp-pkg-json");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "my-app",
  "version": "0.1.0",
  "dependencies": {}
}"#,
        )
        .unwrap();

        let modified =
            stamp_package_json(&root.join("package.json"), "2.0.0", &Default::default()).unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["version"], "2.0.0");
        assert_eq!(parsed["name"], "my-app");
    }

    #[test]
    fn stamp_all_updates_mixed_ecosystem_packages() {
        let root = temp_dir("stamp-all-mixed");
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "rust-app"
version = "0.1.0"
"#,
        )
        .unwrap();
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "web-app",
  "version": "0.1.0"
}"#,
        )
        .unwrap();

        let packages = vec![
            Package {
                id: PackageId::new(Ecosystem::Cargo, "rust-app"),
                name: "rust-app".to_string(),
                ecosystem: Ecosystem::Cargo,
                manifest_path: root.join("Cargo.toml"),
                js_package_manager: None,
                task_opt_ins: BTreeMap::new(),
                bridged_dependencies: vec![],
                internal_dependencies: vec![],
            },
            Package {
                id: PackageId::new(Ecosystem::Js, "web-app"),
                name: "web-app".to_string(),
                ecosystem: Ecosystem::Js,
                manifest_path: root.join("package.json"),
                js_package_manager: None,
                task_opt_ins: BTreeMap::new(),
                bridged_dependencies: vec![],
                internal_dependencies: vec![],
            },
        ];

        let modified = stamp_all(&root, &packages, "3.0.0").unwrap();
        assert_eq!(modified.len(), 2);

        let cargo_content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(cargo_content.contains("version = \"3.0.0\""));

        let pkg_content = fs::read_to_string(root.join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&pkg_content).unwrap();
        assert_eq!(parsed["version"], "3.0.0");
    }

    #[test]
    fn stamps_virtual_workspace_root_cargo_toml() {
        let root = temp_dir("stamp-virtual-ws");
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();

        // No packages discovered from root (virtual workspace has no [package])
        let packages: Vec<Package> = vec![];
        let modified = stamp_all(&root, &packages, "2.0.0").unwrap();
        assert_eq!(modified.len(), 1);

        let content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(content.contains("version = \"2.0.0\""));
        assert!(content.contains("edition = \"2024\""));
    }

    #[test]
    fn skips_uv_packages() {
        let root = temp_dir("stamp-skip-uv");
        fs::write(
            root.join("pyproject.toml"),
            r#"[project]
name = "py-app"
version = "0.1.0"
"#,
        )
        .unwrap();

        let packages = vec![Package {
            id: PackageId::new(Ecosystem::Uv, "py-app"),
            name: "py-app".to_string(),
            ecosystem: Ecosystem::Uv,
            manifest_path: root.join("pyproject.toml"),
            js_package_manager: None,
            task_opt_ins: BTreeMap::new(),
            bridged_dependencies: vec![],
            internal_dependencies: vec![],
        }];

        let modified = stamp_all(&root, &packages, "3.0.0").unwrap();
        assert!(modified.is_empty());

        let content = fs::read_to_string(root.join("pyproject.toml")).unwrap();
        assert!(content.contains("version = \"0.1.0\""));
    }

    #[test]
    fn stamps_sibling_deno_json_version() {
        let root = temp_dir("stamp-deno");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "@myko/rs",
  "version": "0.1.0"
}"#,
        )
        .unwrap();
        fs::write(
            root.join("deno.json"),
            r#"{
  "name": "@myko/rs",
  "version": "0.1.0",
  "exports": "./index.ts"
}"#,
        )
        .unwrap();

        let modified =
            stamp_package_json(&root.join("package.json"), "2.0.0", &Default::default()).unwrap();
        assert!(modified);

        let deno_content = fs::read_to_string(root.join("deno.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&deno_content).unwrap();
        assert_eq!(parsed["version"], "2.0.0");
    }

    #[test]
    fn stamps_deno_json_jsr_import_versions() {
        let root = temp_dir("stamp-deno-imports");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "@myko/ts",
  "version": "0.1.0"
}"#,
        )
        .unwrap();
        fs::write(
            root.join("deno.json"),
            r#"{
  "name": "@myko/ts",
  "version": "0.1.0",
  "imports": {
    "@myko/rs": "jsr:@myko/rs@0.1.0",
    "rxjs": "npm:rxjs@^7.8.1"
  }
}"#,
        )
        .unwrap();

        let selected = ["@myko/rs"].into_iter().collect();
        stamp_package_json(&root.join("package.json"), "2.0.0", &selected).unwrap();

        let deno_content = fs::read_to_string(root.join("deno.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&deno_content).unwrap();
        assert_eq!(parsed["version"], "2.0.0");
        assert_eq!(parsed["imports"]["@myko/rs"], "jsr:@myko/rs@2.0.0");
        // npm specifiers should be untouched
        assert_eq!(parsed["imports"]["rxjs"], "npm:rxjs@^7.8.1");
    }

    #[test]
    fn update_jsr_specifier_replaces_version() {
        assert_eq!(
            update_jsr_specifier("jsr:@myko/rs@0.1.0", "2.0.0"),
            Some("jsr:@myko/rs@2.0.0".to_string())
        );
    }

    #[test]
    fn update_jsr_specifier_ignores_npm() {
        assert_eq!(update_jsr_specifier("npm:rxjs@^7.8.1", "2.0.0"), None);
    }

    #[test]
    fn resolves_workspace_caret_version() {
        assert_eq!(
            resolve_dep_specifier("workspace:^1.0.0", "2.0.0"),
            Some("^2.0.0".to_string())
        );
    }

    #[test]
    fn resolves_workspace_tilde_version() {
        assert_eq!(
            resolve_dep_specifier("workspace:~1.0.0", "2.0.0"),
            Some("~2.0.0".to_string())
        );
    }

    #[test]
    fn resolves_workspace_exact_version() {
        assert_eq!(
            resolve_dep_specifier("workspace:1.0.0", "2.0.0"),
            Some("2.0.0".to_string())
        );
    }

    #[test]
    fn resolves_workspace_star() {
        assert_eq!(
            resolve_dep_specifier("workspace:*", "2.0.0"),
            Some("2.0.0".to_string())
        );
    }

    #[test]
    fn resolves_workspace_caret_shorthand() {
        assert_eq!(
            resolve_dep_specifier("workspace:^", "2.0.0"),
            Some("^2.0.0".to_string())
        );
    }

    #[test]
    fn resolves_workspace_tilde_shorthand() {
        assert_eq!(
            resolve_dep_specifier("workspace:~", "2.0.0"),
            Some("~2.0.0".to_string())
        );
    }

    #[test]
    fn resolves_plain_caret_version() {
        assert_eq!(
            resolve_dep_specifier("^1.0.0", "2.0.0"),
            Some("^2.0.0".to_string())
        );
    }

    #[test]
    fn resolves_plain_tilde_version() {
        assert_eq!(
            resolve_dep_specifier("~1.0.0", "2.0.0"),
            Some("~2.0.0".to_string())
        );
    }

    #[test]
    fn resolves_plain_exact_version() {
        assert_eq!(
            resolve_dep_specifier("1.0.0", "2.0.0"),
            Some("2.0.0".to_string())
        );
    }

    #[test]
    fn ignores_file_specifier() {
        assert_eq!(resolve_dep_specifier("file:../shared", "2.0.0"), None);
    }

    #[test]
    fn ignores_link_specifier() {
        assert_eq!(resolve_dep_specifier("link:../shared", "2.0.0"), None);
    }

    #[test]
    fn resolves_workspace_deps_in_package_json() {
        let root = temp_dir("stamp-pkg-ws-deps");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "@repo/app",
  "version": "1.0.0",
  "dependencies": {
    "@repo/shared": "workspace:^1.0.0",
    "lodash": "^4.17.21"
  },
  "devDependencies": {
    "@repo/tools": "workspace:~1.0.0"
  }
}"#,
        )
        .unwrap();

        let workspace_names: std::collections::HashSet<&str> =
            ["@repo/shared", "@repo/tools"].into_iter().collect();
        let modified =
            stamp_package_json(&root.join("package.json"), "2.0.0", &workspace_names).unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["version"], "2.0.0");
        assert_eq!(parsed["dependencies"]["@repo/shared"], "^2.0.0");
        assert_eq!(parsed["dependencies"]["lodash"], "^4.17.21");
        assert_eq!(parsed["devDependencies"]["@repo/tools"], "~2.0.0");
    }

    #[test]
    fn resolves_plain_semver_deps_for_workspace_packages() {
        let root = temp_dir("stamp-pkg-plain-deps");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "@repo/app",
  "version": "1.0.0",
  "dependencies": {
    "@repo/shared": "^1.0.0",
    "lodash": "^4.17.21"
  }
}"#,
        )
        .unwrap();

        let workspace_names: std::collections::HashSet<&str> =
            ["@repo/shared"].into_iter().collect();
        let modified =
            stamp_package_json(&root.join("package.json"), "2.0.0", &workspace_names).unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["version"], "2.0.0");
        assert_eq!(parsed["dependencies"]["@repo/shared"], "^2.0.0");
        // External dep unchanged
        assert_eq!(parsed["dependencies"]["lodash"], "^4.17.21");
    }

    #[test]
    fn resolves_workspace_shorthand_specifiers() {
        let root = temp_dir("stamp-pkg-ws-shorthand");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "@repo/app",
  "version": "1.0.0",
  "dependencies": {
    "@repo/shared": "workspace:*",
    "@repo/utils": "workspace:^"
  }
}"#,
        )
        .unwrap();

        let workspace_names: std::collections::HashSet<&str> =
            ["@repo/shared", "@repo/utils"].into_iter().collect();
        let modified =
            stamp_package_json(&root.join("package.json"), "2.0.0", &workspace_names).unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["version"], "2.0.0");
        assert_eq!(parsed["dependencies"]["@repo/shared"], "2.0.0");
        assert_eq!(parsed["dependencies"]["@repo/utils"], "^2.0.0");
    }

    #[test]
    fn leaves_file_specifiers_for_workspace_packages() {
        let root = temp_dir("stamp-pkg-file-dep");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "@repo/app",
  "version": "1.0.0",
  "dependencies": {
    "@repo/shared": "file:../shared"
  }
}"#,
        )
        .unwrap();

        let workspace_names: std::collections::HashSet<&str> =
            ["@repo/shared"].into_iter().collect();
        stamp_package_json(&root.join("package.json"), "2.0.0", &workspace_names).unwrap();

        let content = fs::read_to_string(root.join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["dependencies"]["@repo/shared"], "file:../shared");
    }

    #[test]
    fn stamp_all_resolves_js_workspace_deps() {
        let root = temp_dir("stamp-all-js-ws-deps");
        fs::create_dir_all(root.join("packages/shared")).unwrap();
        fs::create_dir_all(root.join("packages/app")).unwrap();

        fs::write(
            root.join("packages/shared/package.json"),
            r#"{
  "name": "@repo/shared",
  "version": "1.0.0"
}"#,
        )
        .unwrap();
        fs::write(
            root.join("packages/app/package.json"),
            r#"{
  "name": "@repo/app",
  "version": "1.0.0",
  "dependencies": {
    "@repo/shared": "workspace:^1.0.0"
  }
}"#,
        )
        .unwrap();

        let packages = vec![
            Package {
                id: PackageId::new(Ecosystem::Js, "@repo/shared"),
                name: "@repo/shared".to_string(),
                ecosystem: Ecosystem::Js,
                manifest_path: root.join("packages/shared/package.json"),
                js_package_manager: None,
                task_opt_ins: BTreeMap::new(),
                bridged_dependencies: vec![],
                internal_dependencies: vec![],
            },
            Package {
                id: PackageId::new(Ecosystem::Js, "@repo/app"),
                name: "@repo/app".to_string(),
                ecosystem: Ecosystem::Js,
                manifest_path: root.join("packages/app/package.json"),
                js_package_manager: None,
                task_opt_ins: BTreeMap::new(),
                bridged_dependencies: vec![],
                internal_dependencies: vec![],
            },
        ];

        let modified = stamp_all(&root, &packages, "2.0.0").unwrap();
        assert_eq!(modified.len(), 2);

        let app_content = fs::read_to_string(root.join("packages/app/package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&app_content).unwrap();
        assert_eq!(parsed["version"], "2.0.0");
        assert_eq!(parsed["dependencies"]["@repo/shared"], "^2.0.0");
    }

    #[test]
    fn excluded_versions_leave_packages_and_dependency_requirements_unchanged() {
        let root = temp_dir("stamp-exclude-version");
        for dir in [
            "crates/private",
            "crates/sdk",
            "crates/app",
            "packages/private",
            "packages/sdk",
            "packages/app",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/*"]

[workspace.dependencies]
private = { version = "0.0.0", path = "crates/private" }
sdk = { version = "1.0.0", path = "crates/sdk" }
"#,
        )
        .unwrap();
        fs::write(
            root.join("crates/private/Cargo.toml"),
            "[package]\nname = \"private\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/sdk/Cargo.toml"),
            "[package]\nname = \"sdk\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("crates/app/Cargo.toml"),
            r#"[package]
name = "app"
version = "1.0.0"

[dependencies]
private = { version = "0.0.0", path = "../private" }
sdk = { version = "1.0.0", path = "../sdk" }
"#,
        )
        .unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"root","private":true,"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        for (name, current) in [("private", "0.0.0"), ("sdk", "1.0.0")] {
            fs::write(
                root.join(format!("packages/{name}/package.json")),
                format!(r#"{{"name":"@repo/{name}","version":"{current}"}}"#),
            )
            .unwrap();
        }
        fs::write(
            root.join("packages/app/package.json"),
            r#"{"name":"@repo/app","version":"1.0.0","dependencies":{"@repo/private":"workspace:0.0.0","@repo/sdk":"workspace:^1.0.0"}}"#,
        )
        .unwrap();

        let packages = crate::manifest::discover_packages(&root).unwrap();
        stamp_selected(
            &root,
            &packages,
            "2.0.0",
            &StampOptions {
                exclude_versions: vec!["0.0.0".into()],
                ..Default::default()
            },
        )
        .unwrap();

        assert!(
            fs::read_to_string(root.join("crates/private/Cargo.toml"))
                .unwrap()
                .contains("version = \"0.0.0\"")
        );
        let cargo_app = fs::read_to_string(root.join("crates/app/Cargo.toml")).unwrap();
        assert!(cargo_app.contains("private = { version = \"0.0.0\""));
        assert!(cargo_app.contains("sdk = { version = \"2.0.0\""));
        let root_cargo = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(root_cargo.contains("private = { version = \"0.0.0\""));
        assert!(root_cargo.contains("sdk = { version = \"2.0.0\""));

        let private_js: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(root.join("packages/private/package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(private_js["version"], "0.0.0");
        let app_js: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(root.join("packages/app/package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(app_js["version"], "2.0.0");
        assert_eq!(app_js["dependencies"]["@repo/private"], "workspace:0.0.0");
        assert_eq!(app_js["dependencies"]["@repo/sdk"], "^2.0.0");
    }

    #[test]
    fn stamp_config_reads_excluded_versions() {
        let root = temp_dir("stamp-config");
        fs::write(
            root.join("flux.toml"),
            "[stamp]\nexclude_versions = [\"0.0.0\", \"0.0.1\"]\n\n[tasks.build]\ndefault = \"true\"\n",
        )
        .unwrap();
        assert_eq!(
            StampConfig::load(&root).unwrap().exclude_versions,
            vec!["0.0.0", "0.0.1"]
        );
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis();
        let path = std::env::temp_dir().join(format!("cargo-flux-stamp-{prefix}-{millis}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
