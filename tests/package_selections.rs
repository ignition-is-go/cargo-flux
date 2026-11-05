use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    root: PathBuf,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("flux-selection-")
            .tempdir()
            .unwrap();
        let root = directory.path().to_path_buf();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers=['app','core','fixture','scratch','sdk']\nresolver='2'\n[workspace.dependencies]\ncore_alias={package='core',path='core'}\nfixture_alias={package='fixture',path='fixture'}\n",
        )
        .unwrap();
        for name in ["app", "core", "fixture", "scratch", "sdk"] {
            fs::create_dir_all(root.join(name).join("src")).unwrap();
            let publish = if name == "sdk" { "" } else { "publish=false\n" };
            let dependencies = match name {
                "app" => {
                    "[dependencies]\ncore_alias={workspace=true}\n[target.'cfg(unix)'.dev-dependencies]\nfixture_alias={workspace=true}\n"
                }
                "sdk" | "scratch" => "[dependencies]\ncore={path='../core'}\n",
                // A test-only cycle must affect tests without becoming a build-order cycle.
                "fixture" => "[dependencies]\napp={path='../app'}\n",
                _ => "",
            };
            fs::write(root.join(name).join("Cargo.toml"), format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n{publish}[package.metadata.flux]\ntasks=['check']\n{dependencies}")).unwrap();
            fs::write(root.join(name).join("src/lib.rs"), "").unwrap();
        }
        fs::write(
            root.join("flux.toml"),
            r#"
[selections.product]
roots = ["cargo:app"]
cargo_publishable = true
dependencies = true

[selections.workspace]
all = true

[selections.bad]
roots = ["cargo:missing"]

[tasks.check]
selection = "product"
autoapply = "none"
cascade = "none"
cargo = ["git", "rev-parse", "--show-prefix"]

[tasks.root-check]
selection = "product"
root = ["git", "--version"]
"#,
        )
        .unwrap();
        let fixture = Self {
            root,
            _directory: directory,
        };
        fixture.git(&["init", "--quiet"]);
        fixture.git(&["config", "user.email", "test@example.com"]);
        fixture.git(&["config", "user.name", "Test"]);
        fixture.commit("base");
        fixture
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "."]);
        self.git(&["commit", "--quiet", "-m", message]);
    }

    fn flux(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cargo-flux"))
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap()
    }

    fn success(&self, args: &[&str]) -> String {
        let output = self.flux(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

#[test]
fn shipment_selection_is_shared_by_query_plan_and_execution() {
    let f = Fixture::new();
    assert_eq!(
        f.success(&["select", "product"]),
        "cargo:app\ncargo:core\ncargo:sdk\n"
    );
    assert_eq!(
        f.success(&["plan", "check", "--stamp-args"]),
        "--package=app\n--package=core\n--package=sdk\n"
    );
    let run = f.success(&["run", "check"]);
    for name in ["app", "core", "sdk"] {
        assert!(run.lines().any(|line| line == format!("{name}/")));
    }
    for name in ["fixture", "scratch"] {
        assert!(!run.lines().any(|line| line == format!("{name}/")));
    }
    let all = f.success(&["plan", "check", "--selection", "workspace", "--stamp-args"]);
    assert!(all.contains("--package=scratch"));
    assert!(all.contains("--package=fixture"));
}

#[test]
fn changed_target_specific_test_dependency_affects_its_consumer_without_shipping_it() {
    let f = Fixture::new();
    fs::write(
        f.root.join("fixture/src/lib.rs"),
        "// changed test support\n",
    )
    .unwrap();
    f.commit("change fixture");
    assert_eq!(
        f.success(&["select", "product", "--affected", "HEAD^"]),
        "cargo:app\n"
    );
    assert_eq!(
        f.success(&["plan", "check", "--affected", "HEAD^", "--stamp-args"]),
        "--package=app\n"
    );
    assert!(f.success(&["topo"]).contains("fixture"));
}

#[test]
fn changes_to_shared_runtime_dependency_do_not_reintroduce_unshipped_consumers() {
    let f = Fixture::new();
    fs::write(f.root.join("core/src/lib.rs"), "// changed runtime\n").unwrap();
    f.commit("change core");
    assert_eq!(
        f.success(&["plan", "check", "--affected", "HEAD^", "--stamp-args"]),
        "--package=app\n--package=core\n--package=sdk\n"
    );
}

#[test]
fn unknown_selection_and_missing_required_root_fail_closed() {
    let f = Fixture::new();
    for args in [
        vec!["plan", "check", "--selection", "missing"],
        vec!["select", "bad"],
    ] {
        let output = f.flux(&args);
        assert!(!output.status.success());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn empty_and_restricted_publish_registries_are_respected() {
    let f = Fixture::new();
    let path = f.root.join("sdk/Cargo.toml");
    let original = fs::read_to_string(&path).unwrap();
    for (publish, included) in [("[]", false), ("false", false), ("['internal']", true)] {
        fs::write(
            &path,
            original.replace("[package]\n", &format!("[package]\npublish={publish}\n")),
        )
        .unwrap();
        assert_eq!(
            f.success(&["select", "product"]).contains("cargo:sdk\n"),
            included
        );
    }
}

#[test]
fn inherited_publish_policy_is_respected() {
    let f = Fixture::new();
    let manifest = f.root.join("Cargo.toml");
    let original = fs::read_to_string(&manifest).unwrap();
    let sdk = f.root.join("sdk/Cargo.toml");
    fs::write(
        &sdk,
        fs::read_to_string(&sdk)
            .unwrap()
            .replace("[package]\n", "[package]\npublish.workspace=true\n"),
    )
    .unwrap();
    for (policy, included) in [("false", false), ("['internal']", true)] {
        fs::write(
            &manifest,
            format!("{original}\n[workspace.package]\npublish={policy}\n"),
        )
        .unwrap();
        assert_eq!(
            f.success(&["select", "product"]).contains("cargo:sdk\n"),
            included
        );
    }
}

#[test]
fn root_tasks_skip_when_no_selected_package_is_affected() {
    let f = Fixture::new();
    fs::write(f.root.join("scratch/src/lib.rs"), "// unshipped change\n").unwrap();
    f.commit("change scratch");
    let output = f.success(&["run", "root-check", "--affected", "HEAD^"]);
    assert!(output.contains("nothing to run"));
    assert!(!output.contains("git version"));
}
