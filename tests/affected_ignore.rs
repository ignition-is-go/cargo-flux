use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn ignored_changes_produce_an_empty_affected_run() {
    let root = temp_dir("ignored-change");
    fs::create_dir_all(root.join("app/src")).expect("create package");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n",
    )
    .expect("write workspace");
    fs::write(
        root.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("write package");
    fs::write(root.join("app/src/lib.rs"), "").expect("write source");
    fs::write(
        root.join("flux.toml"),
        r#"[affected]
ignore = ["AGENTS.md"]

[tasks.check]
autoapply = "all"
cargo = ["touch", "checked"]
"#,
    )
    .expect("write config");
    fs::write(root.join("AGENTS.md"), "initial\n").expect("write ignored file");

    git(&root, &["init", "--quiet"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "base"]);
    fs::write(root.join("AGENTS.md"), "updated\n").expect("update ignored file");
    git(&root, &["add", "AGENTS.md"]);
    git(&root, &["commit", "--quiet", "-m", "docs"]);

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-flux"))
        .args([
            "--root",
            root.to_str().expect("UTF-8 root"),
            "run",
            "check",
            "--affected",
            "HEAD^",
        ])
        .output()
        .expect("run cargo flux");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join("app/checked").exists());
    assert!(String::from_utf8_lossy(&output.stdout).contains("nothing to run"));
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn temp_dir(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "cargo-flux-integration-{prefix}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp root");
    path
}
