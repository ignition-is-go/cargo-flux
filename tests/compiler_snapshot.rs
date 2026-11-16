use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn success(command: &mut Command) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn flux(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-flux"));
    command.arg("--root").arg(root).args(args);
    command
}

fn cargo(root: &Path, home: &Path, args: &[&str]) -> Output {
    let mut command = if args.first() == Some(&"build") {
        let mut command = flux(root, &["storage", "cargo", "--path", "target", "--"]);
        command.args(args);
        command
    } else {
        let mut command = Command::new("cargo");
        command.args(args);
        command
    };
    success(
        command
            .current_dir(root)
            .env("CARGO_HOME", home)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("CARGO_INCREMENTAL", "0")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER"),
    )
}

fn artifacts(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .collect()
}

fn build(root: &Path, home: &Path, feature: bool, fresh: bool) -> Vec<Value> {
    let mut args = vec!["build", "--message-format=json"];
    if feature {
        args.extend(["--features", "extra"]);
    }
    let messages = artifacts(&cargo(root, home, &args));
    assert!(!messages.is_empty());
    if fresh {
        assert!(messages.iter().all(|message| message["fresh"] == true));
    }
    messages
}

fn fingerprint_count(path: &Path) -> usize {
    if path.file_name().is_some_and(|name| name == "fingerprint") {
        return 1;
    }
    let entries = fs::read_dir(path).unwrap();
    if path.file_name().is_some_and(|name| name == ".fingerprint") {
        return entries.count();
    }
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .map(|path| fingerprint_count(&path))
        .sum()
}

#[test]
fn compiler_snapshots_discard_obsolete_graphs_and_keep_both_feature_variants_warm() {
    let temporary = tempfile::Builder::new()
        .prefix("flux compiler snapshot ")
        .tempdir()
        .unwrap();
    let root = temporary.path().join("project");
    let dependency = temporary.path().join("dependency");
    let home = temporary.path().join("cargo-home");
    let store = temporary.path().join("objects");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("stable/src")).unwrap();
    fs::write(
        root.join("stable/Cargo.toml"),
        "[package]\nname='snapshot-stable'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(root.join("stable/src/lib.rs"), "pub fn unchanged() {}\n").unwrap();
    fs::create_dir_all(dependency.join("src")).unwrap();
    fs::write(
        root.join("flux.toml"),
        format!("[cache]\ndirectory={:?}\n", store.to_string_lossy()),
    )
    .unwrap();
    fs::write(
        root.join("src/main.rs"),
        "fn main() { let extra = if cfg!(feature=\"extra\") { 10 } else { 0 }; println!(\"{}\", snapshot_dependency::value() + extra); }",
    )
    .unwrap();
    success(Command::new("git").current_dir(&dependency).args(["init"]));
    let compatibility_binary = std::env::var_os("CARGO_FLUX_COMPAT_BIN");
    let mut previous = Vec::new();
    let mut snapshot_counts = Vec::new();
    let mut previous_key = None;
    for version in 1..=3 {
        fs::write(
            dependency.join("Cargo.toml"),
            format!(
                "[package]\nname='snapshot-dependency'\nversion='0.1.{version}'\nedition='2024'\n"
            ),
        )
        .unwrap();
        fs::write(
            dependency.join("src/lib.rs"),
            format!("pub fn value() -> u32 {{ {version} }}"),
        )
        .unwrap();
        success(
            Command::new("git")
                .current_dir(&dependency)
                .args(["add", "."]),
        );
        success(Command::new("git").current_dir(&dependency).args([
            "-c",
            "user.name=Flux snapshot test",
            "-c",
            "user.email=snapshot@example.invalid",
            "commit",
            "-m",
            "dependency revision",
        ]));
        let revision = success(
            Command::new("git")
                .current_dir(&dependency)
                .args(["rev-parse", "HEAD"]),
        );
        let revision = String::from_utf8(revision.stdout).unwrap();
        let path = dependency.to_string_lossy().replace('\\', "/");
        let url = format!("file://{}{path}", if cfg!(windows) { "/" } else { "" });
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname='snapshot-app'\nversion='0.1.0'\nedition='2024'\n\
                 [workspace]\n[features]\nextra=[]\n\
                 [dependencies]\nsnapshot-stable={{path='stable'}}\nsnapshot-dependency={{git={url:?},rev={:?}}}\n",
                revision.trim()
            ),
        )
        .unwrap();
        let mut current = build(&root, &home, false, false);
        current.extend(build(&root, &home, true, false));
        if version > 1 {
            assert!(
                current
                    .iter()
                    .filter(|artifact| artifact["target"]["name"] == "snapshot_stable")
                    .all(|artifact| artifact["fresh"] == true)
            );
        }
        let key = success(&mut flux(
            &root,
            &["storage", "cargo-key", "--scope", "fixture"],
        ))
        .stdout;
        if let Some(binary) = &compatibility_binary {
            let old_key = success(Command::new(binary).arg("--root").arg(&root).args([
                "storage",
                "cargo-key",
                "--scope",
                "fixture",
            ]))
            .stdout;
            assert_eq!(
                old_key, key,
                "existing compiler cache keys must remain identical"
            );
        }
        if let Some(previous) = previous_key {
            assert_eq!(previous, key);
        }
        previous_key = Some(key);
        let mut save = match (&compatibility_binary, version) {
            (Some(binary), 1) => {
                let mut command = Command::new(binary);
                command.arg("--root").arg(&root);
                command
            }
            _ => {
                let mut command = flux(&root, &[]);
                command.arg("storage").arg("save").arg("--compiler");
                command
            }
        };
        if compatibility_binary.is_some() && version == 1 {
            fs::remove_file(root.join("target/.flux-compiler-units.json")).unwrap();
            save.args(["storage", "save"]);
        }
        success(
            save.args(["--key", "compiler", "--path", "target"])
                .env("CARGO_HOME", &home),
        );
        fs::remove_dir_all(root.join("target")).unwrap();
        let mut restore = match (&compatibility_binary, version) {
            (Some(binary), 2) => {
                let mut command = Command::new(binary);
                command.arg("--root").arg(&root);
                command
            }
            _ => flux(&root, &[]),
        };
        success(restore.args([
            "storage",
            "restore",
            "--key",
            "compiler",
            "--path",
            "target",
            "--required",
        ]));
        for artifact in &current {
            for file in artifact["filenames"].as_array().unwrap() {
                assert!(Path::new(file.as_str().unwrap()).exists());
            }
        }
        for file in &previous {
            assert!(
                !Path::new(file).exists(),
                "obsolete output survived: {file:?}"
            );
        }
        previous = current
            .iter()
            .filter(|artifact| artifact["target"]["name"] == "snapshot_dependency")
            .flat_map(|artifact| artifact["filenames"].as_array().unwrap())
            .map(|file| file.as_str().unwrap().to_owned())
            .collect();
        build(&root, &home, false, true);
        build(&root, &home, true, true);
        snapshot_counts.push(fingerprint_count(&root.join("target")));
        let output = cargo(&root, &home, &["run", "--quiet"]);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            version.to_string()
        );
        let output = cargo(&root, &home, &["run", "--quiet", "--features", "extra"]);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            (version + 10).to_string()
        );
    }
    assert!(
        snapshot_counts.windows(2).all(|pair| pair[0] == pair[1]),
        "{snapshot_counts:?}"
    );
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", snapshot_dependency::value() + 100); }",
    )
    .unwrap();
    let output = cargo(&root, &home, &["run", "--quiet"]);
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "103");
}

#[test]
fn insufficient_restore_space_preserves_existing_outputs() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("flux.toml"), "").unwrap();
    fs::create_dir(root.path().join("dist")).unwrap();
    fs::write(root.path().join("dist/output"), "snapshot").unwrap();
    success(&mut flux(
        root.path(),
        &["storage", "save", "--key", "space", "--path", "dist"],
    ));
    fs::write(root.path().join("dist/output"), "keep me").unwrap();
    let reserve = (u64::MAX / 2).to_string();
    let output = success(&mut flux(
        root.path(),
        &[
            "storage",
            "restore",
            "--key",
            "space",
            "--path",
            "dist",
            "--reserve-bytes",
            &reserve,
        ],
    ));
    assert!(String::from_utf8_lossy(&output.stderr).contains("space"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("storage miss"));
    assert_eq!(
        fs::read_to_string(root.path().join("dist/output")).unwrap(),
        "keep me"
    );
    let output = flux(
        root.path(),
        &[
            "storage",
            "restore",
            "--key",
            "space",
            "--path",
            "dist",
            "--reserve-bytes",
            &reserve,
            "--required",
        ],
    )
    .output()
    .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(root.path().join("dist/output")).unwrap(),
        "keep me"
    );
}

#[test]
fn shared_target_preserves_declared_workspaces_and_build_script_outputs() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let home = root.join("cargo-home");
    fs::write(root.join("flux.toml"), "").unwrap();
    for name in ["first", "second"] {
        let project = root.join(name);
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(
            project.join("Cargo.toml"),
            format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n[workspace]\n"),
        )
        .unwrap();
        fs::write(project.join("build.rs"), "fn main() { let path = std::path::PathBuf::from(std::env::var_os(\"OUT_DIR\").unwrap()); std::fs::write(path.join(\"generated.rs\"), \"pub const VALUE: u32 = 42;\").unwrap(); }").unwrap();
        fs::write(project.join("src/main.rs"), "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\")); fn main() { println!(\"{}\", VALUE); }").unwrap();
        cargo(
            root,
            &home,
            &["build", "--manifest-path", &format!("{name}/Cargo.toml")],
        );
    }
    fs::write(root.join("flux.toml"), "").unwrap();
    success(
        flux(
            root,
            &[
                "storage",
                "save",
                "--compiler",
                "--key",
                "shared",
                "--path",
                "target",
                "--manifest-path",
                "first/Cargo.toml",
                "--manifest-path",
                "second/Cargo.toml",
            ],
        )
        .env("CARGO_HOME", &home),
    );
    fs::remove_dir_all(root.join("target")).unwrap();
    success(&mut flux(
        root,
        &[
            "storage",
            "restore",
            "--key",
            "shared",
            "--path",
            "target",
            "--required",
        ],
    ));
    for lock in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
        assert!(
            !root.join("target/debug").join(lock).exists(),
            "coordination locks must not be archived"
        );
    }
    for name in ["first", "second"] {
        let output = cargo(
            root,
            &home,
            &[
                "build",
                "--manifest-path",
                &format!("{name}/Cargo.toml"),
                "--message-format=json",
            ],
        );
        let artifacts = artifacts(&output);
        assert!(!artifacts.is_empty());
        assert!(
            artifacts.iter().all(|artifact| artifact["fresh"] == true),
            "{artifacts:?}"
        );
        let output = cargo(
            root,
            &home,
            &[
                "run",
                "--quiet",
                "--manifest-path",
                &format!("{name}/Cargo.toml"),
            ],
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "42");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn rejected_download_releases_space_before_fallback() {
    use sha2::{Digest, Sha256};
    // A bounded tmpfs makes admission deterministic while other builds use the root disk.
    let temporary = tempfile::Builder::new()
        .prefix("flux-fallback-")
        .tempdir_in("/dev/shm")
        .unwrap();
    let root = temporary.path();
    assert!(fs2::available_space(root).unwrap() > 48 * 1024 * 1024);
    fs::write(root.join("flux.toml"), "").unwrap();
    fs::create_dir(root.join("dist")).unwrap();
    let mut noise = Vec::with_capacity(8 * 1024 * 1024);
    for counter in 0_u64..(8 * 1024 * 1024 / 32) {
        noise.extend(Sha256::digest(counter.to_le_bytes()));
    }
    let mut large = noise.clone();
    large.resize(32 * 1024 * 1024, 0);
    fs::write(root.join("dist/data"), large).unwrap();
    success(flux(root, &["storage", "save", "--key", "big", "--path", "dist"]).env("TMPDIR", root));
    fs::write(root.join("dist/data"), &noise[..3 * 1024 * 1024]).unwrap();
    success(
        flux(
            root,
            &["storage", "save", "--key", "small", "--path", "dist"],
        )
        .env("TMPDIR", root),
    );
    fs::remove_dir_all(root.join("dist")).unwrap();
    let reserve = (fs2::available_space(root).unwrap() - 10 * 1024 * 1024).to_string();
    let result = success(
        flux(
            root,
            &[
                "storage",
                "restore",
                "--key",
                "big",
                "--key",
                "small",
                "--path",
                "dist",
                "--reserve-bytes",
                &reserve,
            ],
        )
        .env("TMPDIR", root),
    );
    assert!(
        String::from_utf8_lossy(&result.stdout).contains("storage hit: small"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read(root.join("dist/data")).unwrap(),
        &noise[..3 * 1024 * 1024]
    );
}

#[test]
fn local_version_changes_keep_snapshots_bounded_and_feature_variants_warm() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let home = root.join("cargo-home");
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("flux.toml"), "").unwrap();
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", env!(\"CARGO_PKG_VERSION\")); }",
    )
    .unwrap();
    let mut counts = Vec::new();
    for version in 1..=3 {
        fs::write(root.join("Cargo.toml"), format!("[package]\nname='local-version'\nversion='0.1.{version}'\nedition='2024'\n[workspace]\n[features]\nextra=[]\n")).unwrap();
        if version == 2 {
            std::thread::scope(|scope| {
                let default = scope.spawn(|| build(root, &home, false, false));
                let extra = scope.spawn(|| build(root, &home, true, false));
                default.join().unwrap();
                extra.join().unwrap();
            });
        } else {
            build(root, &home, false, false);
            build(root, &home, true, false);
        }
        success(&mut flux(
            root,
            &[
                "storage",
                "save",
                "--compiler",
                "--key",
                "local",
                "--path",
                "target",
            ],
        ));
        fs::remove_dir_all(root.join("target")).unwrap();
        success(&mut flux(
            root,
            &[
                "storage",
                "restore",
                "--key",
                "local",
                "--path",
                "target",
                "--required",
            ],
        ));
        counts.push(fingerprint_count(&root.join("target")));
        build(root, &home, false, true);
        // A later job using only the default feature set must preserve the other warm variant.
        success(&mut flux(
            root,
            &[
                "storage",
                "save",
                "--compiler",
                "--key",
                "local",
                "--path",
                "target",
            ],
        ));
        fs::remove_dir_all(root.join("target")).unwrap();
        success(&mut flux(
            root,
            &[
                "storage",
                "restore",
                "--key",
                "local",
                "--path",
                "target",
                "--required",
            ],
        ));
        build(root, &home, true, true);
        let result = cargo(root, &home, &["run", "--quiet"]);
        assert_eq!(
            String::from_utf8(result.stdout).unwrap().trim(),
            format!("0.1.{version}")
        );
    }
    assert_eq!(counts, vec![2, 2, 2]);
}

#[test]
fn failed_build_preserves_previous_compiler_provenance() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let home = root.join("cargo-home");
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("flux.toml"), "").unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='failed-build'\nversion='0.1.0'\nedition='2024'\n[workspace]\n",
    )
    .unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let output = cargo(root, &home, &["build"]);
    assert!(
        output.stdout.is_empty(),
        "default Cargo output should retain its human format"
    );
    let recorded = fs::read(root.join("target/.flux-compiler-units.json")).unwrap();
    fs::write(root.join("src/main.rs"), "invalid rust\n").unwrap();
    let result = flux(
        root,
        &["storage", "cargo", "--path", "target", "--", "build"],
    )
    .env("CARGO_HOME", home)
    .env("CARGO_TARGET_DIR", root.join("target"))
    .output()
    .unwrap();
    assert!(!result.status.success());
    assert_eq!(
        recorded,
        fs::read(root.join("target/.flux-compiler-units.json")).unwrap()
    );
}

#[test]
fn recorded_check_and_test_units_remain_warm() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let home = root.join("cargo-home");
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("flux.toml"), "").unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='compile-modes'\nversion='0.1.0'\nedition='2024'\n[workspace]\n",
    )
    .unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    for args in [&["check"][..], &["test", "--no-run"][..]] {
        success(
            flux(root, &["storage", "cargo", "--path", "target", "--"])
                .args(args)
                .env("CARGO_HOME", &home)
                .env("CARGO_INCREMENTAL", "0")
                .env("CARGO_TARGET_DIR", root.join("target")),
        );
    }
    success(&mut flux(
        root,
        &[
            "storage",
            "save",
            "--compiler",
            "--key",
            "modes",
            "--path",
            "target",
        ],
    ));
    fs::remove_dir_all(root.join("target")).unwrap();
    success(&mut flux(
        root,
        &[
            "storage",
            "restore",
            "--key",
            "modes",
            "--path",
            "target",
            "--required",
        ],
    ));
    for args in [
        &["check", "--message-format=json"][..],
        &["test", "--no-run", "--message-format=json"][..],
    ] {
        let result = cargo(root, &home, args);
        let artifacts = artifacts(&result);
        assert!(!artifacts.is_empty());
        assert!(
            artifacts.iter().all(|artifact| artifact["fresh"] == true),
            "{artifacts:?}"
        );
    }
}

#[test]
fn local_patch_version_changes_discard_previous_compiler_units() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let home = root.join("cargo-home");
    fs::create_dir(root.join("src")).unwrap();
    fs::create_dir_all(root.join("patch/src")).unwrap();
    fs::write(root.join("flux.toml"), "").unwrap();
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", patched_local::value()); }\n",
    )
    .unwrap();
    let mut counts = Vec::new();
    for version in 1..=3 {
        fs::write(root.join("Cargo.toml"), format!("[package]\nname='patch-consumer'\nversion='0.1.0'\nedition='2024'\n[workspace]\n[dependencies]\npatched-local='=0.1.{version}'\n[patch.crates-io]\npatched-local={{path='patch'}}\n")).unwrap();
        fs::write(root.join("patch/Cargo.toml"), format!("[package]\nname='patched-local'\nversion='0.1.{version}'\nedition='2024'\n[workspace]\n")).unwrap();
        fs::write(
            root.join("patch/src/lib.rs"),
            format!("pub fn value() -> u32 {{ {version} }}\n"),
        )
        .unwrap();
        cargo(
            root,
            &home,
            &["build", "--offline", "--message-format=json"],
        );
        success(&mut flux(
            root,
            &[
                "storage",
                "save",
                "--compiler",
                "--key",
                "patch",
                "--path",
                "target",
            ],
        ));
        fs::remove_dir_all(root.join("target")).unwrap();
        success(&mut flux(
            root,
            &[
                "storage",
                "restore",
                "--key",
                "patch",
                "--path",
                "target",
                "--required",
            ],
        ));
        counts.push(fingerprint_count(&root.join("target")));
        let output = cargo(
            root,
            &home,
            &["build", "--offline", "--message-format=json"],
        );
        assert!(
            artifacts(&output)
                .iter()
                .all(|artifact| artifact["fresh"] == true)
        );
        let output = cargo(root, &home, &["run", "--offline", "--quiet"]);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            version.to_string()
        );
    }
    assert_eq!(counts, vec![2, 2, 2]);
}

#[test]
fn unknown_vendor_units_do_not_protect_obsolete_recorded_versions() {
    use sha2::{Digest, Sha256};
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let home = root.join("cargo-home");
    fs::create_dir(root.join("src")).unwrap();
    fs::create_dir(root.join(".cargo")).unwrap();
    fs::create_dir_all(root.join("patch/src")).unwrap();
    fs::create_dir_all(root.join("vendor/vendor-parent/src")).unwrap();
    fs::write(root.join("flux.toml"), "").unwrap();
    fs::write(
        root.join(".cargo/config.toml"),
        "[source.crates-io]\nreplace-with='vendored'\n[source.vendored]\ndirectory='vendor'\n",
    )
    .unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname='vendor-consumer'\nversion='0.1.0'\nedition='2024'\n[workspace]\n[dependencies]\nvendor-parent='1.0'\n[patch.crates-io]\npatched-local={path='patch'}\n").unwrap();
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", vendor_parent::value()); }\n",
    )
    .unwrap();
    let vendor = root.join("vendor/vendor-parent");
    fs::write(vendor.join("Cargo.toml"), "[package]\nname='vendor-parent'\nversion='1.0.0'\nedition='2024'\n[dependencies]\npatched-local='0.1'\n").unwrap();
    fs::write(
        vendor.join("src/lib.rs"),
        "pub fn value() -> u32 { patched_local::value() }\n",
    )
    .unwrap();
    let checksums = ["Cargo.toml", "src/lib.rs"]
        .into_iter()
        .map(|path| {
            (
                path,
                format!("{:x}", Sha256::digest(fs::read(vendor.join(path)).unwrap())),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    fs::write(
        vendor.join(".cargo-checksum.json"),
        serde_json::to_vec(&serde_json::json!({"files":checksums,"package":null})).unwrap(),
    )
    .unwrap();
    let mut counts = Vec::new();
    for version in 1..=2 {
        fs::write(root.join("patch/Cargo.toml"), format!("[package]\nname='patched-local'\nversion='0.1.{version}'\nedition='2024'\n[workspace]\n")).unwrap();
        fs::write(
            root.join("patch/src/lib.rs"),
            format!("pub fn value() -> u32 {{ {version} }}\n"),
        )
        .unwrap();
        cargo(
            root,
            &home,
            &["build", "--offline", "--message-format=json"],
        );
        success(&mut flux(
            root,
            &[
                "storage",
                "save",
                "--compiler",
                "--key",
                "vendor",
                "--path",
                "target",
            ],
        ));
        fs::remove_dir_all(root.join("target")).unwrap();
        success(&mut flux(
            root,
            &[
                "storage",
                "restore",
                "--key",
                "vendor",
                "--path",
                "target",
                "--required",
            ],
        ));
        counts.push(fingerprint_count(&root.join("target")));
        let output = cargo(
            root,
            &home,
            &["build", "--offline", "--message-format=json"],
        );
        assert!(
            artifacts(&output)
                .iter()
                .all(|artifact| artifact["fresh"] == true)
        );
        let output = cargo(root, &home, &["run", "--offline", "--quiet"]);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            version.to_string()
        );
    }
    assert_eq!(counts, vec![3, 3]);
}
