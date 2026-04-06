//! Version stamping for workspace manifests

use anyhow::{Context, Result};
use std::path::Path;

use crate::manifest::{Ecosystem, Package};

/// Stamp a version string into all discovered workspace packages.
/// Returns the list of files that were modified.
pub fn stamp_all(packages: &[Package], version: &str) -> Result<Vec<String>> {
    let mut modified = Vec::new();

    for package in packages {
        let path = &package.manifest_path;
        let was_modified = match package.ecosystem {
            Ecosystem::Cargo => stamp_cargo_toml(path, version)?,
            Ecosystem::Js => stamp_package_json(path, version)?,
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
    if let Some(pkg) = doc.get_mut("package").and_then(|p| p.as_table_mut()) {
        if pkg.contains_key("version") {
            pkg["version"] = toml_edit::value(version);
            modified = true;
        }
    }

    // Update intra-workspace path dependency versions
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = doc.get_mut(section).and_then(|d| d.as_table_mut()) {
            for (_key, dep) in deps.iter_mut() {
                if let Some(table) = dep.as_inline_table_mut() {
                    if table.contains_key("path") && table.contains_key("version") {
                        table.insert("version", toml_edit::value(version).into_value().unwrap());
                        modified = true;
                    }
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

/// Stamp version into a package.json file.
fn stamp_package_json(path: &Path, version: &str) -> Result<bool> {
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

    let output = serde_json::to_string_pretty(&json)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    std::fs::write(path, format!("{}\n", output))
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(true)
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

        let modified = stamp_package_json(&root.join("package.json"), "2.0.0").unwrap();
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

        let modified = stamp_all(&packages, "3.0.0").unwrap();
        assert_eq!(modified.len(), 2);

        let cargo_content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(cargo_content.contains("version = \"3.0.0\""));

        let pkg_content = fs::read_to_string(root.join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&pkg_content).unwrap();
        assert_eq!(parsed["version"], "3.0.0");
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

        let modified = stamp_all(&packages, "3.0.0").unwrap();
        assert!(modified.is_empty());

        let content = fs::read_to_string(root.join("pyproject.toml")).unwrap();
        assert!(content.contains("version = \"0.1.0\""));
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
