//! Version stamping for workspace manifests

use anyhow::{Context, Result};
use std::path::Path;

use crate::manifest::{Ecosystem, Package};

/// Stamp a version string into all discovered workspace packages
/// and the root workspace Cargo.toml if it exists.
/// Returns the list of files that were modified.
pub fn stamp_all(root: &Path, packages: &[Package], version: &str) -> Result<Vec<String>> {
    let mut modified = Vec::new();
    let mut stamped_paths = std::collections::HashSet::new();

    let js_package_names: std::collections::HashSet<&str> = packages
        .iter()
        .filter(|p| p.ecosystem == Ecosystem::Js)
        .map(|p| p.name.as_str())
        .collect();

    // Stamp root Cargo.toml (handles virtual workspaces with [workspace.package] version)
    let root_cargo = root.join("Cargo.toml");
    if root_cargo.exists() && stamp_cargo_toml(&root_cargo, version)? {
        modified.push(root_cargo.display().to_string());
        stamped_paths.insert(root_cargo);
    }

    for package in packages {
        let path = &package.manifest_path;
        if stamped_paths.contains(path) {
            continue;
        }
        let was_modified = match package.ecosystem {
            Ecosystem::Cargo => stamp_cargo_toml(path, version)?,
            Ecosystem::Js => stamp_package_json(path, version, &js_package_names)?,
            Ecosystem::Uv => false,
        };
        if was_modified {
            modified.push(path.display().to_string());
        }
    }

    Ok(modified)
}

/// Stamp version into a Cargo.toml file, preserving formatting.
fn stamp_cargo_toml(path: &Path, version: &str) -> Result<bool> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let mut modified = false;

    // Update [package] version
    if let Some(pkg) = doc.get_mut("package").and_then(|p| p.as_table_mut())
        && pkg.contains_key("version")
    {
        pkg["version"] = toml_edit::value(version);
        modified = true;
    }

    // Update [workspace.package] version
    if let Some(ws_pkg) = doc
        .get_mut("workspace")
        .and_then(|w| w.as_table_mut())
        .and_then(|w| w.get_mut("package"))
        .and_then(|p| p.as_table_mut())
        && ws_pkg.contains_key("version")
    {
        ws_pkg["version"] = toml_edit::value(version);
        modified = true;
    }

    // Update intra-workspace path dependency versions
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = doc.get_mut(section).and_then(|d| d.as_table_mut()) {
            stamp_path_dep_versions(deps, version, &mut modified);
        }
    }

    // Update [workspace.dependencies] path dependency versions
    if let Some(ws_deps) = doc
        .get_mut("workspace")
        .and_then(|w| w.as_table_mut())
        .and_then(|w| w.get_mut("dependencies"))
        .and_then(|d| d.as_table_mut())
    {
        stamp_path_dep_versions(ws_deps, version, &mut modified);
    }

    // Update workspace dependency versions in non-inline tables too
    // (toml_edit distinguishes inline `{ version = "...", path = "..." }` from multi-line tables)
    fn stamp_path_dep_versions(deps: &mut toml_edit::Table, version: &str, modified: &mut bool) {
        for (_key, dep) in deps.iter_mut() {
            if let Some(table) = dep.as_inline_table_mut() {
                if table.contains_key("path") && table.contains_key("version") {
                    table.insert("version", toml_edit::value(version).into_value().unwrap());
                    *modified = true;
                }
            } else if let Some(table) = dep.as_table_mut()
                && table.contains_key("path")
                && table.contains_key("version")
            {
                table["version"] = toml_edit::value(version);
                *modified = true;
            }
        }
    }

    if modified {
        std::fs::write(path, doc.to_string())
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(modified)
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
    for section in ["dependencies", "devDependencies", "peerDependencies"] {
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
            stamp_deno_json(&deno_path, version)?;
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
fn stamp_deno_json(path: &Path, version: &str) -> Result<()> {
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

        stamp_package_json(&root.join("package.json"), "2.0.0", &Default::default()).unwrap();

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
