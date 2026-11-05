use std::fs;
use std::process::{Command, Output};

struct Fixture {
    root: tempfile::TempDir,
    s3_prefix: Option<String>,
}

impl Fixture {
    fn new(s3: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut fixture = Self {
            root,
            s3_prefix: None,
        };
        fixture.git(&["init", "--quiet"]);
        fixture.git(&["config", "flux.cache-version", "one"]);
        let command = serde_json::to_string(&vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .to_string(),
            "--exact".into(),
            "cache_producer".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ])
        .unwrap();
        let store = if s3 {
            let endpoint =
                std::env::var("FLUX_TEST_S3_ENDPOINT").expect("set FLUX_TEST_S3_ENDPOINT");
            let bucket = std::env::var("FLUX_TEST_S3_BUCKET")
                .expect("set FLUX_TEST_S3_BUCKET to a dedicated test bucket");
            let prefix = format!(
                "tests/{}-{}",
                std::process::id(),
                fixture.root.path().file_name().unwrap().to_string_lossy()
            );
            fixture.s3_prefix = Some(prefix.clone());
            format!(
                "[cache.s3]\nendpoint={endpoint:?}\nbucket={bucket:?}\nprefix={prefix:?}\ntimeout_seconds=10\n"
            )
        } else {
            String::new()
        };
        fs::write(fixture.root.path().join("flux.toml"), format!(r#"
{store}
[tasks.produce]
root = {command}
outputs = {{ result = "stdout" }}
cache = {{ inputs = ["source"], paths = ["dist"], env = ["CACHE_TEST_MODE"], tools = [["git", "config", "--get", "flux.cache-version"]] }}

[tasks.consume]
depends_on = ["produce"]
when = {{ output = "produce.result", nonempty = true }}
root = ["git", "--version"]
"#)).unwrap();
        if let Some(source) = std::env::var_os("FLUX_TEST_CANARY_INPUT") {
            fs::copy(source, fixture.root.path().join("source")).unwrap();
        } else {
            fs::write(fixture.root.path().join("source"), "rship canary input\n").unwrap();
        }
        fixture
    }

    fn git(&self, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(self.root.path())
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    fn run(&self, mode: &str, no_cache: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-flux"));
        command
            .arg("--root")
            .arg(self.root.path())
            .args(["run", "consume", "--report"])
            .arg(self.root.path().join("report.json"))
            .env("FLUX_CACHE_PRODUCER", "1")
            .env("CACHE_TEST_MODE", mode);
        if no_cache {
            command.arg("--no-cache");
        }
        command.output().unwrap()
    }

    fn success(&self, mode: &str, no_cache: bool) -> String {
        let output = self.run(mode, no_cache);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("cache save failed"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn count(&self) -> u32 {
        fs::read_to_string(self.root.path().join("executions"))
            .unwrap()
            .parse()
            .unwrap()
    }
    fn clear_outputs(&self) {
        fs::remove_dir_all(self.root.path().join("dist")).unwrap();
    }

    fn aws(&self, args: &[&str]) -> Output {
        Command::new("aws")
            .arg("--endpoint-url")
            .arg(std::env::var("FLUX_TEST_S3_ENDPOINT").unwrap())
            .args(["--cli-connect-timeout", "5", "--cli-read-timeout", "10"])
            .args(args)
            .env("AWS_PAGER", "")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(prefix) = &self.s3_prefix {
            if std::env::var("FLUX_TEST_S3_KEEP").as_deref() == Ok("1") {
                eprintln!(
                    "Kept MinIO canary: s3://{}/{prefix}/",
                    std::env::var("FLUX_TEST_S3_BUCKET").unwrap()
                );
                return;
            }
            let uri = format!(
                "s3://{}/{prefix}/",
                std::env::var("FLUX_TEST_S3_BUCKET").unwrap()
            );
            let output = self.aws(&["s3", "rm", &uri, "--recursive", "--only-show-errors"]);
            assert!(
                output.status.success(),
                "failed to remove owned S3 test prefix"
            );
        }
    }
}

// Child process used as a portable artifact producer. Running ignored tests
// manually without the fixture flag has no filesystem side effects.
#[test]
#[ignore = "fixture subprocess only"]
fn cache_producer() {
    if std::env::var("FLUX_CACHE_PRODUCER").as_deref() != Ok("1") {
        return;
    }
    let count: u32 = fs::read_to_string("executions")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(0);
    fs::write("executions", (count + 1).to_string()).unwrap();
    fs::create_dir_all("dist").unwrap();
    fs::copy("source", "dist/result").unwrap();
    fs::write("dist/mode", std::env::var("CACHE_TEST_MODE").unwrap()).unwrap();
    if std::path::Path::new("mutate").exists() {
        fs::write("source", "changed during build").unwrap();
    }
    assert!(
        !std::path::Path::new("fail").exists(),
        "intentional producer failure"
    );
    println!("generated");
}

#[test]
fn restores_artifacts_and_stdout_without_rerunning_and_removes_stale_outputs() {
    let f = Fixture::new(false);
    assert!(f.success("a", false).contains("cache miss: produce"));
    let first: serde_json::Value =
        serde_json::from_slice(&fs::read(f.root.path().join("report.json")).unwrap()).unwrap();
    fs::write(f.root.path().join("dist/stale"), "stale").unwrap();
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert!(!f.root.path().join("dist/stale").exists());
    assert_eq!(f.count(), 1);
    let second: serde_json::Value =
        serde_json::from_slice(&fs::read(f.root.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(first["task_outputs"], second["task_outputs"]);
    f.clear_outputs();
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert_eq!(
        fs::read_to_string(f.root.path().join("dist/result")).unwrap(),
        "rship canary input\n"
    );
    assert_eq!(f.count(), 1);
}

#[test]
fn input_environment_tool_and_command_changes_invalidate_cached_results() {
    let f = Fixture::new(false);
    f.success("a", false);
    fs::write(f.root.path().join("source"), "changed\n").unwrap();
    assert!(f.success("a", false).contains("cache miss: produce"));
    assert!(f.success("b", false).contains("cache miss: produce"));
    f.git(&["config", "flux.cache-version", "two"]);
    assert!(f.success("b", false).contains("cache miss: produce"));
    let config = f.root.path().join("flux.toml");
    fs::write(
        &config,
        fs::read_to_string(&config)
            .unwrap()
            .replace("--nocapture", "--show-output"),
    )
    .unwrap();
    assert!(f.success("b", false).contains("cache miss: produce"));
    assert_eq!(f.count(), 5);
}

#[test]
fn no_cache_executes_without_reading_or_writing_cache() {
    let f = Fixture::new(false);
    f.success("a", true);
    assert!(!f.root.path().join(".flux/cache").exists());
    f.success("a", false);
    f.success("a", true);
    assert_eq!(f.count(), 3);
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert_eq!(f.count(), 3);
}

#[test]
fn file_only_cache_hits_without_captured_stdout() {
    let f = Fixture::new(false);
    let config = f.root.path().join("flux.toml");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("outputs = { result = \"stdout\" }\n", "")
        .replace(
            "when = { output = \"produce.result\", nonempty = true }\n",
            "",
        );
    fs::write(config, text).unwrap();
    f.success("a", false);
    f.clear_outputs();
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert_eq!(f.count(), 1);
}

#[test]
fn unrelated_root_outputs_do_not_invalidate_cached_task() {
    let f = Fixture::new(false);
    f.git(&["config", "flux.unrelated", "one"]);
    let config = f.root.path().join("flux.toml");
    let text = fs::read_to_string(&config).unwrap().replace(
        "depends_on = [\"produce\"]",
        "depends_on = [\"unrelated\", \"produce\"]",
    );
    fs::write(config, format!("{text}\n[tasks.unrelated]\nroot=['git','config','--get','flux.unrelated']\noutputs={{noise='stdout'}}\n")).unwrap();
    f.success("a", false);
    f.git(&["config", "flux.unrelated", "two"]);
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert_eq!(f.count(), 1);
}

#[test]
fn corrupt_archive_falls_back_to_execution_and_repairs_cache() {
    let f = Fixture::new(false);
    f.success("a", false);
    let archive = fs::read_dir(f.root.path().join(".flux/cache"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(archive, "interrupted archive").unwrap();
    f.clear_outputs();
    let output = f.run("a", false);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cache restore failed"));
    assert_eq!(f.count(), 2);
    f.clear_outputs();
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert_eq!(f.count(), 2);
}

#[test]
fn failed_tasks_do_not_publish_partial_outputs() {
    let f = Fixture::new(false);
    fs::write(f.root.path().join("fail"), "").unwrap();
    assert!(!f.run("a", false).status.success());
    assert!(!f.root.path().join(".flux/cache").exists());
    fs::remove_file(f.root.path().join("fail")).unwrap();
    assert!(f.success("a", false).contains("cache miss: produce"));
    f.clear_outputs();
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert_eq!(f.count(), 2);
}

#[test]
fn inputs_changed_during_execution_are_not_saved_under_an_old_key() {
    let f = Fixture::new(false);
    fs::write(f.root.path().join("mutate"), "").unwrap();
    let output = f.run("a", false);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("inputs changed during execution"));
    assert!(!f.root.path().join(".flux/cache").exists());
}

#[test]
#[cfg(unix)]
fn stalled_partial_s3_transfers_are_bounded_and_do_not_replace_outputs() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(false);
    let config = f.root.path().join("flux.toml");
    fs::write(
        &config,
        format!(
            "{}\n[cache.s3]\nbucket='fixture'\nendpoint='http://127.0.0.1:9'\ntimeout_seconds=1\n",
            fs::read_to_string(&config).unwrap()
        ),
    )
    .unwrap();
    let bin = f.root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let fake = bin.join("aws");
    fs::write(&fake, "#!/bin/sh\nfor last do :; done\ncase \"$last\" in s3://*) ;; *) printf partial > \"$last\" ;; esac\nexec sleep 10\n").unwrap();
    fs::set_permissions(fake, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-flux"))
        .arg("--root")
        .arg(f.root.path())
        .args(["run", "produce"])
        .env("PATH", path)
        .env("FLUX_CACHE_PRODUCER", "1")
        .env("CACHE_TEST_MODE", "a")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(started.elapsed().as_secs() < 6);
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeded 1 seconds"));
    assert_eq!(
        fs::read_to_string(f.root.path().join("dist/result")).unwrap(),
        "rship canary input\n"
    );
    assert_eq!(f.count(), 1);
}

#[test]
#[ignore = "requires dedicated MinIO bucket and standard AWS credential environment"]
fn minio_restores_and_rejects_truncated_remote_artifacts() {
    let f = Fixture::new(true);
    assert!(f.success("a", false).contains("cache saved: produce"));
    f.clear_outputs();
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert_eq!(f.count(), 1);
    assert_eq!(
        fs::read(f.root.path().join("source")).unwrap(),
        fs::read(f.root.path().join("dist/result")).unwrap()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(f.root.path().join("source"))
                .unwrap()
                .permissions()
                .mode(),
            fs::metadata(f.root.path().join("dist/result"))
                .unwrap()
                .permissions()
                .mode()
        );
    }
    let bucket = std::env::var("FLUX_TEST_S3_BUCKET").unwrap();
    let objects = f.aws(&[
        "s3api",
        "list-objects-v2",
        "--bucket",
        &bucket,
        "--prefix",
        f.s3_prefix.as_ref().unwrap(),
    ]);
    assert!(objects.status.success());
    let objects: serde_json::Value = serde_json::from_slice(&objects.stdout).unwrap();
    let key = objects["Contents"][0]["Key"].as_str().unwrap();
    let broken = f.root.path().join("truncated.tar");
    fs::write(&broken, "interrupted transfer").unwrap();
    assert!(
        f.aws(&[
            "s3api",
            "put-object",
            "--bucket",
            &bucket,
            "--key",
            key,
            "--body",
            broken.to_str().unwrap()
        ])
        .status
        .success()
    );
    f.clear_outputs();
    let output = f.run("a", false);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cache restore failed"));
    assert_eq!(f.count(), 2);
    f.clear_outputs();
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert_eq!(f.count(), 2);
}
