use crate::plugins::default_plugins;
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ecosystem {
    Cargo,
    Js,
    Uv,
}

impl Display for Ecosystem {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Ecosystem::Cargo => write!(f, "cargo"),
            Ecosystem::Js => write!(f, "js"),
            Ecosystem::Uv => write!(f, "uv"),
        }
    }
}

impl std::str::FromStr for Ecosystem {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cargo" => Ok(Ecosystem::Cargo),
            "js" => Ok(Ecosystem::Js),
            "uv" => Ok(Ecosystem::Uv),
            _ => bail!("unknown ecosystem `{value}`"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum JsPackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl Display for JsPackageManager {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            JsPackageManager::Npm => write!(f, "npm"),
            JsPackageManager::Pnpm => write!(f, "pnpm"),
            JsPackageManager::Yarn => write!(f, "yarn"),
            JsPackageManager::Bun => write!(f, "bun"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageId {
    ecosystem: Ecosystem,
    name: String,
}

impl PackageId {
    pub fn new(ecosystem: Ecosystem, name: impl Into<String>) -> Self {
        Self {
            ecosystem,
            name: name.into(),
        }
    }
}

impl Display for PackageId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.ecosystem, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BridgeTarget {
    pub ecosystem: Option<Ecosystem>,
    pub package_name: String,
}

impl BridgeTarget {
    pub fn parse(value: &str) -> Result<Self> {
        if let Some((prefix, package_name)) = value.split_once(':') {
            if let Ok(ecosystem) = prefix.parse::<Ecosystem>() {
                return Ok(Self {
                    ecosystem: Some(ecosystem),
                    package_name: package_name.to_string(),
                });
            }
        }

        Ok(Self {
            ecosystem: None,
            package_name: value.to_string(),
        })
    }
}

impl Display for BridgeTarget {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.ecosystem {
            Some(ecosystem) => write!(f, "{}:{}", ecosystem, self.package_name),
            None => write!(f, "{}", self.package_name),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Package {
    pub id: PackageId,
    pub name: String,
    pub ecosystem: Ecosystem,
    pub manifest_path: PathBuf,
    pub js_package_manager: Option<JsPackageManager>,
    pub task_opt_ins: BTreeMap<String, TaskOptIn>,
    pub bridged_dependencies: Vec<BridgeTarget>,
    pub internal_dependencies: Vec<PackageId>,
}

impl Package {
    pub fn opts_into_task(&self, task: &str) -> bool {
        self.task_opt_ins.contains_key(task)
    }

    pub fn task_variables(&self, task: &str) -> Option<&BTreeMap<String, String>> {
        self.task_opt_ins.get(task).map(|opt_in| &opt_in.variables)
    }

    pub fn display_label(&self) -> String {
        match (self.ecosystem, self.js_package_manager) {
            (Ecosystem::Js, Some(manager)) => manager.to_string(),
            _ => self.ecosystem.to_string(),
        }
    }

    pub fn colored_display_label(&self, use_color: bool) -> String {
        colorize_display_label(
            &self.display_label(),
            self.ecosystem,
            self.js_package_manager,
            use_color,
        )
    }
}

pub fn colorize_display_label(
    label: &str,
    ecosystem: Ecosystem,
    js_package_manager: Option<JsPackageManager>,
    use_color: bool,
) -> String {
    if !use_color {
        return label.to_string();
    }

    colorize_display_label_with_style(label, ecosystem, js_package_manager, false)
}

pub fn colorize_display_label_dimmed(
    label: &str,
    ecosystem: Ecosystem,
    js_package_manager: Option<JsPackageManager>,
    use_color: bool,
) -> String {
    if !use_color {
        return label.to_string();
    }

    colorize_display_label_with_style(label, ecosystem, js_package_manager, true)
}

fn colorize_display_label_with_style(
    label: &str,
    ecosystem: Ecosystem,
    js_package_manager: Option<JsPackageManager>,
    dimmed: bool,
) -> String {
    let prefix = if dimmed { "2;" } else { "1;" };

    let color = match (ecosystem, js_package_manager) {
        (Ecosystem::Cargo, _) => 208,
        (Ecosystem::Uv, _) => 42,
        (Ecosystem::Js, Some(JsPackageManager::Npm)) => 196,
        (Ecosystem::Js, Some(JsPackageManager::Pnpm)) => 220,
        (Ecosystem::Js, Some(JsPackageManager::Yarn)) => 39,
        (Ecosystem::Js, Some(JsPackageManager::Bun)) => 212,
        (Ecosystem::Js, None) => 45,
    };

    format!("\x1b[{prefix}38;5;{color}m{label}\x1b[0m")
}

#[derive(Debug, Clone, Default)]
pub struct TaskOptIn {
    pub variables: BTreeMap<String, String>,
}

#[derive(Debug)]
pub(crate) struct RawPackage {
    pub(crate) id: PackageId,
    pub(crate) name: String,
    pub(crate) ecosystem: Ecosystem,
    pub(crate) manifest_path: PathBuf,
    pub(crate) js_package_manager: Option<JsPackageManager>,
    pub(crate) task_opt_ins: BTreeMap<String, TaskOptIn>,
    pub(crate) bridged_dependencies: Vec<BridgeTarget>,
    pub(crate) declared_dependencies: Vec<String>,
    pub(crate) warned_declared_dependencies: Vec<WarnedDependencyRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarnedDependencyRef {
    pub name: String,
    pub section: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkspaceWarning {
    pub package_id: PackageId,
    pub dependency_id: PackageId,
    pub section: String,
}

impl Display for WorkspaceWarning {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "warning: package `{}` references workspace package `{}` in special dependency section `{}`",
            self.package_id, self.dependency_id, self.section
        )
    }
}

#[derive(Debug)]
pub struct WorkspaceDiscovery {
    pub packages: Vec<Package>,
    pub warnings: Vec<WorkspaceWarning>,
}

pub fn discover_workspace(root: &Path) -> Result<WorkspaceDiscovery> {
    let plugins = default_plugins();
    let raw_packages = plugins
        .iter()
        .map(|plugin| {
            plugin
                .discover_manifests(root)?
                .into_iter()
                .map(|path| plugin.parse_manifest(&path))
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    Ok(resolve_internal_dependencies(raw_packages))
}

#[cfg(test)]
pub fn discover_packages(root: &Path) -> Result<Vec<Package>> {
    Ok(discover_workspace(root)?.packages)
}

fn resolve_internal_dependencies(raw_packages: Vec<RawPackage>) -> WorkspaceDiscovery {
    let names_by_ecosystem = raw_packages.iter().fold(
        BTreeMap::<Ecosystem, std::collections::BTreeSet<String>>::new(),
        |mut acc, pkg| {
            acc.entry(pkg.id.ecosystem)
                .or_default()
                .insert(pkg.name.clone());
            acc
        },
    );

    let mut warnings = BTreeSet::new();
    let packages = raw_packages
        .into_iter()
        .map(|pkg| {
            let candidates = names_by_ecosystem.get(&pkg.id.ecosystem);
            warnings.extend(pkg.warned_declared_dependencies.iter().filter_map(|dep| {
                candidates
                    .is_some_and(|names| names.contains(&dep.name))
                    .then(|| WorkspaceWarning {
                        package_id: pkg.id.clone(),
                        dependency_id: PackageId::new(pkg.id.ecosystem, dep.name.clone()),
                        section: dep.section.clone(),
                    })
            }));
            let internal_dependencies = pkg
                .declared_dependencies
                .into_iter()
                .filter(|dep| candidates.is_some_and(|names| names.contains(dep)))
                .map(|dep| PackageId::new(pkg.id.ecosystem, dep))
                .collect::<Vec<_>>();

            Package {
                id: pkg.id,
                name: pkg.name,
                ecosystem: pkg.ecosystem,
                manifest_path: pkg.manifest_path,
                js_package_manager: pkg.js_package_manager,
                task_opt_ins: pkg.task_opt_ins,
                bridged_dependencies: pkg.bridged_dependencies,
                internal_dependencies,
            }
        })
        .collect();

    WorkspaceDiscovery {
        packages,
        warnings: warnings.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{BridgeTarget, Ecosystem, JsPackageManager, discover_packages, discover_workspace};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn discovers_internal_dependencies_across_supported_ecosystems() {
        let root = temp_dir("cross-ecosystem");
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "rust-app"
version = "0.1.0"

[dependencies]
serde = "1"
"#,
        )
        .expect("write cargo");

        fs::write(
            root.join("package.json"),
            r#"{
  "name": "web-app",
  "dependencies": {
    "left-pad": "^1.0.0"
  }
}"#,
        )
        .expect("write package");

        fs::write(
            root.join("pyproject.toml"),
            r#"[project]
name = "py-app"
dependencies = ["requests>=2.0"]
"#,
        )
        .expect("write pyproject");

        let packages = discover_packages(&root).expect("discover packages");
        assert_eq!(packages.len(), 3);
        assert!(packages.iter().any(|pkg| pkg.ecosystem == Ecosystem::Cargo));
        assert!(packages.iter().any(|pkg| pkg.ecosystem == Ecosystem::Js));
        assert!(packages.iter().any(|pkg| pkg.ecosystem == Ecosystem::Uv));
    }

    #[test]
    fn links_internal_workspace_dependencies() {
        let root = temp_dir("workspace-links");
        fs::create_dir_all(root.join("shared")).expect("create shared dir");
        fs::create_dir_all(root.join("service")).expect("create service dir");

        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["shared", "service"]
"#,
        )
        .expect("write root workspace");

        fs::write(
            root.join("shared/Cargo.toml"),
            r#"[package]
name = "shared"
version = "0.1.0"
"#,
        )
        .expect("write shared");

        fs::write(
            root.join("service/Cargo.toml"),
            r#"[package]
name = "service"
version = "0.1.0"

[dependencies]
shared = { path = "../shared" }
serde = "1"
"#,
        )
        .expect("write service");

        let packages = discover_packages(&root).expect("discover packages");
        let service = packages
            .into_iter()
            .find(|pkg| pkg.name == "service")
            .expect("service package");

        assert_eq!(service.internal_dependencies.len(), 1);
        assert_eq!(service.internal_dependencies[0].to_string(), "cargo:shared");
    }

    #[test]
    fn ignores_dev_only_dependencies() {
        let root = temp_dir("ignore-dev-deps");
        fs::create_dir_all(root.join("shared")).expect("create shared dir");
        fs::create_dir_all(root.join("service")).expect("create service dir");

        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["shared", "service"]
"#,
        )
        .expect("write root workspace");

        fs::write(
            root.join("shared/Cargo.toml"),
            r#"[package]
name = "shared"
version = "0.1.0"
"#,
        )
        .expect("write shared");

        fs::write(
            root.join("service/Cargo.toml"),
            r#"[package]
name = "service"
version = "0.1.0"

[dev-dependencies]
shared = { path = "../shared" }
"#,
        )
        .expect("write service");

        let packages = discover_packages(&root).expect("discover packages");
        let service = packages
            .into_iter()
            .find(|pkg| pkg.name == "service")
            .expect("service package");

        assert!(service.internal_dependencies.is_empty());
    }

    #[test]
    fn warns_for_workspace_packages_in_cargo_special_dependency_sections() {
        let root = temp_dir("warn-cargo-special-deps");
        fs::create_dir_all(root.join("shared")).expect("create shared dir");
        fs::create_dir_all(root.join("service")).expect("create service dir");
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["shared", "service"]
"#,
        )
        .expect("write workspace");
        fs::write(
            root.join("shared/Cargo.toml"),
            r#"[package]
name = "shared"
version = "0.1.0"
"#,
        )
        .expect("write shared");
        fs::write(
            root.join("service/Cargo.toml"),
            r#"[package]
name = "service"
version = "0.1.0"

[build-dependencies]
shared = { path = "../shared" }

[dev-dependencies]
shared = { path = "../shared" }
"#,
        )
        .expect("write service");

        let discovery = discover_workspace(&root).expect("discover workspace");
        let warnings = discovery
            .warnings
            .into_iter()
            .map(|warning| warning.to_string())
            .collect::<Vec<_>>();

        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("build-dependencies"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("dev-dependencies"))
        );
    }

    #[test]
    fn warns_for_workspace_packages_in_js_dev_dependencies() {
        let root = temp_dir("warn-js-dev-deps");
        fs::create_dir_all(root.join("packages/app")).expect("create app dir");
        fs::create_dir_all(root.join("packages/shared")).expect("create shared dir");

        fs::write(
            root.join("package.json"),
            r#"{
  "name": "root",
  "workspaces": ["packages/*"]
}"#,
        )
        .expect("write root package");
        fs::write(
            root.join("packages/shared/package.json"),
            r#"{
  "name": "@repo/shared"
}"#,
        )
        .expect("write shared package");
        fs::write(
            root.join("packages/app/package.json"),
            r#"{
  "name": "@repo/app",
  "devDependencies": {
    "@repo/shared": "workspace:*"
  }
}"#,
        )
        .expect("write app package");

        let discovery = discover_workspace(&root).expect("discover workspace");
        let warnings = discovery
            .warnings
            .into_iter()
            .map(|warning| warning.to_string())
            .collect::<Vec<_>>();

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("devDependencies"));
        assert!(warnings[0].contains("js:@repo/app"));
        assert!(warnings[0].contains("js:@repo/shared"));
    }

    #[test]
    fn detects_pnpm_from_package_manager_field() {
        let root = temp_dir("detect-pnpm-field");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "web-app",
  "packageManager": "pnpm@9.0.0"
}"#,
        )
        .expect("write package");

        let package = discover_packages(&root)
            .expect("discover packages")
            .into_iter()
            .find(|pkg| pkg.name == "web-app")
            .expect("web package");

        assert_eq!(package.js_package_manager, Some(JsPackageManager::Pnpm));
    }

    #[test]
    fn detects_yarn_from_lockfile() {
        let root = temp_dir("detect-yarn-lockfile");
        fs::write(root.join("yarn.lock"), "").expect("write lockfile");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "web-app"
}"#,
        )
        .expect("write package");

        let package = discover_packages(&root)
            .expect("discover packages")
            .into_iter()
            .find(|pkg| pkg.name == "web-app")
            .expect("web package");

        assert_eq!(package.js_package_manager, Some(JsPackageManager::Yarn));
    }

    #[test]
    fn respects_cargo_workspace_members() {
        let root = temp_dir("cargo-workspace-members");
        fs::create_dir_all(root.join("crates/included")).expect("create included dir");
        fs::create_dir_all(root.join("crates/ignored")).expect("create ignored dir");

        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/included"]
"#,
        )
        .expect("write root cargo");
        fs::write(
            root.join("crates/included/Cargo.toml"),
            r#"[package]
name = "included"
version = "0.1.0"
"#,
        )
        .expect("write included");
        fs::write(
            root.join("crates/ignored/Cargo.toml"),
            r#"[package]
name = "ignored"
version = "0.1.0"
"#,
        )
        .expect("write ignored");

        let packages = discover_packages(&root).expect("discover packages");
        let mut names = packages.into_iter().map(|pkg| pkg.name).collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["included"]);
    }

    #[test]
    fn respects_package_json_workspaces() {
        let root = temp_dir("npm-workspace-members");
        fs::create_dir_all(root.join("packages/included")).expect("create included dir");
        fs::create_dir_all(root.join("packages/ignored")).expect("create ignored dir");

        fs::write(
            root.join("package.json"),
            r#"{
  "name": "root",
  "workspaces": ["packages/included"]
}"#,
        )
        .expect("write root package");
        fs::write(
            root.join("packages/included/package.json"),
            r#"{
  "name": "included"
}"#,
        )
        .expect("write included");
        fs::write(
            root.join("packages/ignored/package.json"),
            r#"{
  "name": "ignored"
}"#,
        )
        .expect("write ignored");

        let packages = discover_packages(&root).expect("discover packages");
        let mut names = packages.into_iter().map(|pkg| pkg.name).collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["included", "root"]);
    }

    #[test]
    fn ignores_node_modules_when_expanding_workspaces() {
        let root = temp_dir("ignore-node-modules");
        fs::create_dir_all(root.join("packages/included")).expect("create included dir");
        fs::create_dir_all(root.join("node_modules/fake-package"))
            .expect("create fake package dir");

        fs::write(
            root.join("package.json"),
            r#"{
  "name": "root",
  "workspaces": ["**"]
}"#,
        )
        .expect("write root package");
        fs::write(
            root.join("packages/included/package.json"),
            r#"{
  "name": "included"
}"#,
        )
        .expect("write included");
        fs::write(
            root.join("node_modules/fake-package/package.json"),
            r#"{
  "name": "fake-package"
}"#,
        )
        .expect("write fake package");

        let packages = discover_packages(&root).expect("discover packages");
        let mut names = packages.into_iter().map(|pkg| pkg.name).collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["included", "root"]);
    }

    #[test]
    fn respects_uv_workspace_members() {
        let root = temp_dir("uv-workspace-members");
        fs::create_dir_all(root.join("packages/included")).expect("create included dir");
        fs::create_dir_all(root.join("packages/ignored")).expect("create ignored dir");

        fs::write(
            root.join("pyproject.toml"),
            r#"[project]
name = "root"
version = "0.1.0"

[tool.uv.workspace]
members = ["packages/included"]
"#,
        )
        .expect("write root pyproject");
        fs::write(
            root.join("packages/included/pyproject.toml"),
            r#"[project]
name = "included"
version = "0.1.0"
"#,
        )
        .expect("write included");
        fs::write(
            root.join("packages/ignored/pyproject.toml"),
            r#"[project]
name = "ignored"
version = "0.1.0"
"#,
        )
        .expect("write ignored");

        let packages = discover_packages(&root).expect("discover packages");
        let mut names = packages.into_iter().map(|pkg| pkg.name).collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["included", "root"]);
    }

    #[test]
    fn reads_cargo_task_opt_ins() {
        let root = temp_dir("cargo-task-opt-ins");
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "rust-app"
version = "0.1.0"

[package.metadata.flux]
tasks = ["build", "test"]
"#,
        )
        .expect("write cargo");

        let package = discover_packages(&root)
            .expect("discover packages")
            .into_iter()
            .find(|pkg| pkg.name == "rust-app")
            .expect("rust package");

        assert!(package.opts_into_task("build"));
        assert!(package.opts_into_task("test"));
        assert!(!package.opts_into_task("lint"));
    }

    #[test]
    fn reads_package_json_task_opt_ins() {
        let root = temp_dir("npm-task-opt-ins");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "web-app",
  "flux": {
    "tasks": ["build", "lint"]
  }
}"#,
        )
        .expect("write package");

        let package = discover_packages(&root)
            .expect("discover packages")
            .into_iter()
            .find(|pkg| pkg.name == "web-app")
            .expect("web package");

        assert!(package.opts_into_task("build"));
        assert!(package.opts_into_task("lint"));
        assert!(!package.opts_into_task("test"));
    }

    #[test]
    fn package_json_build_script_does_not_opt_into_build() {
        let root = temp_dir("npm-build-script-no-opt-in");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "web-app",
  "scripts": {
    "build": "vite build"
  }
}"#,
        )
        .expect("write package");

        let package = discover_packages(&root)
            .expect("discover packages")
            .into_iter()
            .find(|pkg| pkg.name == "web-app")
            .expect("web package");

        assert!(!package.opts_into_task("build"));
    }

    #[test]
    fn reads_pyproject_task_opt_ins() {
        let root = temp_dir("uv-task-opt-ins");
        fs::write(
            root.join("pyproject.toml"),
            r#"[project]
name = "py-app"
version = "0.1.0"

[tool.flux]
tasks = ["test"]
"#,
        )
        .expect("write pyproject");

        let package = discover_packages(&root)
            .expect("discover packages")
            .into_iter()
            .find(|pkg| pkg.name == "py-app")
            .expect("py package");

        assert!(package.opts_into_task("test"));
        assert!(!package.opts_into_task("build"));
    }

    #[test]
    fn reads_package_level_bridges() {
        let root = temp_dir("package-level-bridges");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "web-app",
  "flux": {
    "tasks": ["build"],
    "bridges": ["codegen"]
  }
}"#,
        )
        .expect("write package");

        let package = discover_packages(&root)
            .expect("discover packages")
            .into_iter()
            .find(|pkg| pkg.name == "web-app")
            .expect("web package");

        assert_eq!(
            package.bridged_dependencies,
            vec![BridgeTarget {
                ecosystem: None,
                package_name: "codegen".to_string(),
            }]
        );
    }

    #[test]
    fn reads_ecosystem_scoped_bridges() {
        let root = temp_dir("scoped-package-bridges");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "@scope/ui",
  "flux": {
    "bridges": ["cargo:rust-bridge"]
  }
}"#,
        )
        .expect("write package");

        let package = discover_packages(&root)
            .expect("discover packages")
            .into_iter()
            .find(|pkg| pkg.name == "@scope/ui")
            .expect("web package");

        assert_eq!(
            package.bridged_dependencies,
            vec![BridgeTarget {
                ecosystem: Some(Ecosystem::Cargo),
                package_name: "rust-bridge".to_string(),
            }]
        );
    }

    #[test]
    fn reads_task_variables_from_package_manifest() {
        let root = temp_dir("task-variable-opt-in");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "web-app",
  "flux": {
    "tasks": {
      "build": {
        "variables": {
          "mode": "production"
        }
      }
    }
  }
}"#,
        )
        .expect("write package");

        let package = discover_packages(&root)
            .expect("discover packages")
            .into_iter()
            .find(|pkg| pkg.name == "web-app")
            .expect("web package");

        assert_eq!(
            package
                .task_opt_ins
                .get("build")
                .map(|task| task.variables.clone()),
            Some(BTreeMap::from([(
                "mode".to_string(),
                "production".to_string(),
            )]))
        );
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should work")
            .as_millis();
        let path = std::env::temp_dir().join(format!("cargo-flux-{prefix}-{millis}"));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }
}
