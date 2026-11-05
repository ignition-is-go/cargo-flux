use std::fs;
use std::process::Command;

fn flux(root: &std::path::Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-flux"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn artifact_cli_works_without_a_language_workspace() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let objects = tempfile::tempdir().unwrap();
    let config = format!(
        "[cache]\ndirectory={:?}\n",
        objects.path().to_string_lossy()
    );
    fs::write(source.path().join("flux.toml"), &config).unwrap();
    fs::write(destination.path().join("flux.toml"), config).unwrap();
    fs::create_dir(source.path().join("dist")).unwrap();
    fs::write(source.path().join("dist/binary"), "compiled output").unwrap();
    flux(
        source.path(),
        &[
            "storage",
            "save",
            "--key",
            "artifacts/run-1/linux",
            "--path",
            "dist",
        ],
    );
    flux(
        destination.path(),
        &[
            "storage",
            "restore",
            "--key",
            "artifacts/run-1/linux",
            "--path",
            "dist",
            "--required",
        ],
    );
    assert_eq!(
        fs::read(destination.path().join("dist/binary")).unwrap(),
        b"compiled output"
    );
}

#[test]
#[ignore = "explicit local MinIO storage canary"]
fn native_s3_compressed_artifact_round_trip() {
    use sha2::{Digest, Sha256};
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    let prefix = format!(
        "storage-tests/{}-{}",
        std::process::id(),
        source.path().file_name().unwrap().to_string_lossy()
    );
    let config = format!(
        "[cache.s3]\nendpoint_env='FLUX_TEST_S3_ENDPOINT'\nbucket_env='FLUX_TEST_S3_BUCKET'\nprefix={prefix:?}\ntimeout_seconds=60\n"
    );
    fs::write(source.path().join("flux.toml"), &config).unwrap();
    fs::write(destination.path().join("flux.toml"), config).unwrap();
    fs::create_dir(source.path().join("dist")).unwrap();
    let mut bytes = Vec::with_capacity(10 * 1024 * 1024);
    for counter in 0_u64..(10 * 1024 * 1024 / 32) {
        bytes.extend(Sha256::digest(counter.to_le_bytes()));
    }
    fs::write(source.path().join("dist/binary"), &bytes).unwrap();
    flux(
        source.path(),
        &[
            "storage",
            "save",
            "--key",
            "artifacts/run-1/linux",
            "--path",
            "dist",
        ],
    );
    flux(
        destination.path(),
        &[
            "storage",
            "restore",
            "--key",
            "artifacts/run-1/linux",
            "--path",
            "dist",
            "--required",
        ],
    );
    assert_eq!(
        fs::read(destination.path().join("dist/binary")).unwrap(),
        bytes
    );
    eprintln!("Kept native compressed storage canary prefix: {prefix}");
}

#[test]
fn restored_cargo_intermediates_remain_subject_to_cargo_freshness_checks() {
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    fs::create_dir_all(root.join("app/src")).unwrap();
    fs::create_dir_all(root.join("shared/src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['app','shared']\nresolver='2'\n",
    )
    .unwrap();
    fs::write(root.join("flux.toml"), "").unwrap();
    fs::write(root.join("app/Cargo.toml"), "[package]\nname='flux-cache-app'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nflux-cache-shared={path='../shared'}\n").unwrap();
    fs::write(
        root.join("shared/Cargo.toml"),
        "[package]\nname='flux-cache-shared'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(
        root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", flux_cache_shared::value()); }\n",
    )
    .unwrap();
    fs::write(
        root.join("shared/src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    )
    .unwrap();
    let cargo = |args: &[&str]| {
        let output = Command::new("cargo")
            .current_dir(root)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    cargo(&["build", "--offline", "--verbose"]);
    flux(
        root,
        &[
            "storage",
            "save",
            "--key",
            "compatible-compiler",
            "--path",
            "target",
        ],
    );
    fs::remove_dir_all(root.join("target")).unwrap();
    flux(
        root,
        &[
            "storage",
            "restore",
            "--key",
            "compatible-compiler",
            "--path",
            "target",
            "--required",
        ],
    );
    let fresh = cargo(&["build", "--offline", "--verbose"]);
    let log = String::from_utf8_lossy(&fresh.stderr);
    assert!(log.contains("Fresh flux-cache-shared"), "{log}");
    assert!(log.contains("Fresh flux-cache-app"), "{log}");
    fs::write(
        root.join("shared/src/lib.rs"),
        "pub fn value() -> u32 { 2000 }\n",
    )
    .unwrap();
    let rebuilt = cargo(&["run", "--offline", "--quiet", "-p", "flux-cache-app"]);
    assert_eq!(String::from_utf8(rebuilt.stdout).unwrap().trim(), "2000");
}

#[test]
fn named_artifacts_restore_into_a_staging_directory() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("flux.toml"),
        "[artifacts]\nlinux=['dist']\n",
    )
    .unwrap();
    fs::create_dir(root.path().join("dist")).unwrap();
    fs::write(root.path().join("dist/binary"), "binary").unwrap();
    flux(
        root.path(),
        &[
            "storage",
            "save",
            "--key",
            "run/123/linux",
            "--artifact",
            "linux",
        ],
    );
    flux(
        root.path(),
        &[
            "storage",
            "restore",
            "--key",
            "run/123/linux",
            "--artifact",
            "linux",
            "--destination",
            "prebuilt",
            "--required",
        ],
    );
    assert_eq!(
        fs::read(root.path().join("prebuilt/dist/binary")).unwrap(),
        b"binary"
    );
}

#[test]
fn source_timestamp_restore_preserves_unchanged_sources_and_invalidates_older_changed_inputs() {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    let root = tempfile::tempdir().unwrap();
    for args in [
        &["init", "--quiet"][..],
        &["config", "user.email", "test@example.com"],
        &["config", "user.name", "Test"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(root.path())
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(root.path().join("flux.toml"), "").unwrap();
    let unchanged = root.path().join("unchanged.rs");
    let changed = root.path().join("changed.rs");
    fs::write(&unchanged, "one").unwrap();
    fs::write(&changed, "two").unwrap();
    assert!(
        Command::new("git")
            .current_dir(root.path())
            .args(["add", "."])
            .status()
            .unwrap()
            .success()
    );
    let original = unchanged.metadata().unwrap().modified().unwrap();
    flux(root.path(), &["storage", "sources"]);
    let old = UNIX_EPOCH + Duration::from_secs(100);
    fs::File::open(&unchanged)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();
    fs::write(&changed, "different").unwrap();
    fs::File::open(&changed)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old))
        .unwrap();
    let before = SystemTime::now();
    flux(root.path(), &["storage", "sources", "--restore"]);
    assert_eq!(unchanged.metadata().unwrap().modified().unwrap(), original);
    assert!(changed.metadata().unwrap().modified().unwrap() >= before);
}

#[test]
fn compiler_compatibility_ignores_source_changes_but_partitions_flags_and_purpose() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("flux.toml"), "").unwrap();
    let key = |scope: &str, flags: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-flux"))
            .arg("--root")
            .arg(root.path())
            .args(["storage", "cargo-key", "--scope", scope])
            .env("RUSTFLAGS", flags)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let before = key("validation", "");
    fs::write(root.path().join("Cargo.lock"), "changed source identity").unwrap();
    assert_eq!(before, key("validation", ""));
    assert_ne!(before, key("release", ""));
    assert_ne!(before, key("validation", "-C opt-level=1"));
}
