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
    success(
        Command::new("cargo")
            .current_dir(root)
            .env("CARGO_HOME", home)
            .env("CARGO_TARGET_DIR", root.join("target"))
            .env("CARGO_INCREMENTAL", "0")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .args(args),
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
