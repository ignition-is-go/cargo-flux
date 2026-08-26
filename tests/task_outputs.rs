use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn skipped_root_gate_suppresses_package_execution() {
    let root = temp_dir("package-root-gate");
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
        r#"[tasks.probe]
root = ["printf", ""]
outputs = { value = "stdout" }

[tasks.gate]
depends_on = ["probe"]
when = { output = "probe.value", nonempty = true }
root = ["touch", "gate-ran"]

[tasks.package-check]
depends_on = ["gate"]
autoapply = "all"
cargo = ["touch", "package-ran"]
"#,
    )
    .expect("write config");

    let report = root.join("report.json");
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-flux"))
        .args([
            "--root",
            root.to_str().expect("UTF-8 root"),
            "run",
            "package-check",
            "--report",
            report.to_str().expect("UTF-8 report"),
        ])
        .output()
        .expect("run cargo flux");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join("gate-ran").exists());
    assert!(!root.join("app/package-ran").exists());
    let report = fs::read_to_string(report).expect("read report");
    assert!(report.contains(r#""outcome":"skipped""#));
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
