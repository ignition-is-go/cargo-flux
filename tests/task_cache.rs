#[path = "support/s3.rs"]
mod mock_s3;

use futures_util::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt};

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
            std::env::var("FLUX_TEST_S3_ENDPOINT").expect("set FLUX_TEST_S3_ENDPOINT");
            std::env::var("FLUX_TEST_S3_BUCKET")
                .expect("set FLUX_TEST_S3_BUCKET to a dedicated test bucket");
            let prefix = format!(
                "tests/{}-{}",
                std::process::id(),
                fixture.root.path().file_name().unwrap().to_string_lossy()
            );
            fixture.s3_prefix = Some(prefix.clone());
            format!(
                "[cache.s3]\nendpoint_env='FLUX_TEST_S3_ENDPOINT'\nbucket_env='FLUX_TEST_S3_BUCKET'\nprefix={prefix:?}\ntimeout_seconds=10\n"
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
        self.run_flags(mode, if no_cache { &["--no-cache"] } else { &[] })
    }

    fn run_flags(&self, mode: &str, flags: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-flux"));
        command
            .arg("--root")
            .arg(self.root.path())
            .args(["run", "consume", "--report"])
            .arg(self.root.path().join("report.json"))
            .env("FLUX_CACHE_PRODUCER", "1")
            .env("CACHE_TEST_MODE", mode);
        command.args(flags).output().unwrap()
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

    fn store(&self) -> object_store::aws::AmazonS3 {
        let endpoint = std::env::var("FLUX_TEST_S3_ENDPOINT").unwrap();
        object_store::aws::AmazonS3Builder::new()
            .with_bucket_name(std::env::var("FLUX_TEST_S3_BUCKET").unwrap())
            .with_endpoint(&endpoint)
            .with_allow_http(endpoint.starts_with("http://"))
            .with_virtual_hosted_style_request(false)
            .with_access_key_id(std::env::var("CARGO_FLUX_S3_ACCESS_KEY_ID").unwrap())
            .with_secret_access_key(std::env::var("CARGO_FLUX_S3_SECRET_ACCESS_KEY").unwrap())
            .with_region(
                std::env::var("CARGO_FLUX_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
            )
            .with_client_options(
                object_store::ClientOptions::new().with_timeout(std::time::Duration::from_secs(5)),
            )
            .with_retry(object_store::RetryConfig {
                max_retries: 0,
                ..Default::default()
            })
            .with_allow_http(endpoint.starts_with("http://"))
            .build()
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
            runtime().block_on(async {
                let store = self.store();
                let prefix = object_store::path::Path::from(prefix.clone());
                if let Ok(objects) = store.list(Some(&prefix)).try_collect::<Vec<_>>().await {
                    for object in objects {
                        let _ = store.delete(&object.location).await;
                    }
                }
            });
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
    if std::path::Path::new("block-cache").exists() {
        fs::write(".flux", "block store creation").unwrap();
    }
    println!("generated");
}

#[test]
fn restores_intermediates_but_reruns_commands_and_removes_stale_outputs() {
    let f = Fixture::new(false);
    assert!(f.success("a", false).contains("cache miss: produce"));
    let first: serde_json::Value =
        serde_json::from_slice(&fs::read(f.root.path().join("report.json")).unwrap()).unwrap();
    fs::write(f.root.path().join("dist/stale"), "stale").unwrap();
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert!(!f.root.path().join("dist/stale").exists());
    assert_eq!(f.count(), 2);
    let second: serde_json::Value =
        serde_json::from_slice(&fs::read(f.root.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(first["task_outputs"], second["task_outputs"]);
    f.clear_outputs();
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert_eq!(
        fs::read_to_string(f.root.path().join("dist/result")).unwrap(),
        "rship canary input\n"
    );
    assert_eq!(f.count(), 3);
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
fn declared_environment_prefixes_invalidate_results() {
    let f = Fixture::new(false);
    let config = f.root.path().join("flux.toml");
    fs::write(
        &config,
        fs::read_to_string(&config)
            .unwrap()
            .replace("env = [\"CACHE_TEST_MODE\"]", "env = [\"CACHE_TEST_*\"]"),
    )
    .unwrap();
    f.success("one", false);
    assert!(f.success("two", false).contains("cache miss: produce"));
    assert_eq!(f.count(), 2);
    assert!(f.success("two", false).contains("cache hit: produce"));
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
    assert_eq!(f.count(), 4);
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
    assert_eq!(f.count(), 2);
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
    assert_eq!(f.count(), 2);
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
    assert_eq!(f.count(), 3);
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
    assert_eq!(f.count(), 3);
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
fn stalled_partial_s3_transfers_are_bounded_and_do_not_replace_outputs() {
    let server = mock_s3::Server::new(true);
    let f = Fixture::new(false);
    configure_mock_store(&f, &server.endpoint, 1);
    let started = std::time::Instant::now();
    let output = native_command(&f).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(started.elapsed().as_secs() < 6);
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeded 1 seconds"));
    assert_eq!(
        fs::read_to_string(f.root.path().join("dist/result")).unwrap(),
        "rship canary input\n"
    );
    assert_eq!(f.count(), 1);
}

#[test]
#[ignore = "requires dedicated MinIO bucket and CARGO_FLUX_S3_* credentials"]
fn minio_restores_and_rejects_truncated_remote_artifacts() {
    let f = Fixture::new(true);
    let first = f.run_flags("a", &["--cache-required"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("cache saved: produce"));
    f.clear_outputs();
    let hit = f.run_flags("a", &["--cache-hit-required"]);
    assert!(
        hit.status.success(),
        "{}",
        String::from_utf8_lossy(&hit.stderr)
    );
    assert!(String::from_utf8_lossy(&hit.stdout).contains("cache hit: produce"));
    assert_eq!(f.count(), 2);
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
    runtime().block_on(async {
        let store = f.store();
        let prefix = object_store::path::Path::from(f.s3_prefix.as_ref().unwrap().clone());
        let objects = store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let key = &objects
            .iter()
            .find(|object| object.location.as_ref().ends_with(".tar"))
            .unwrap()
            .location;
        store
            .put(key, b"interrupted transfer".to_vec().into())
            .await
            .unwrap();
    });
    f.clear_outputs();
    let output = f.run("a", false);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cache restore failed"));
    assert_eq!(f.count(), 3);
    f.clear_outputs();
    assert!(f.success("a", false).contains("cache hit: produce"));
    assert_eq!(f.count(), 4);
}

#[test]
fn required_cache_errors_are_not_hidden_by_successful_task_execution() {
    let f = Fixture::new(false);
    fs::write(f.root.path().join("block-cache"), "").unwrap();
    let output = f.run_flags("a", &["--cache-required"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("required cache save failed"));
    assert_eq!(f.count(), 1);
}

#[test]
fn required_cache_rejects_restore_and_prepare_errors_before_running() {
    let f = Fixture::new(false);
    f.success("a", false);
    let archive = fs::read_dir(f.root.path().join(".flux/cache"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(archive, "corrupt").unwrap();
    assert!(!f.run_flags("a", &["--cache-required"]).status.success());
    assert_eq!(f.count(), 1);
    let config = f.root.path().join("flux.toml");
    fs::write(
        &config,
        fs::read_to_string(&config)
            .unwrap()
            .replace("inputs = [\"source\"]", "inputs = [\"missing-input\"]"),
    )
    .unwrap();
    let output = f.run_flags("a", &["--cache-required"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("required cache unavailable"));
    assert_eq!(f.count(), 1);
    assert!(
        !f.run_flags("a", &["--no-cache", "--cache-required"])
            .status
            .success()
    );
}

#[test]
fn missing_environment_store_configuration_is_visible_and_no_cache_can_bypass_it() {
    let f = Fixture::new(false);
    let config = f.root.path().join("flux.toml");
    fs::write(
        &config,
        format!(
            "{}\n[cache.s3]\nbucket_env='FLUX_DELIBERATELY_MISSING_BUCKET_01A1A316'\n",
            fs::read_to_string(&config).unwrap()
        ),
    )
    .unwrap();
    let output = f.run_flags("a", &["--cache-required"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("FLUX_DELIBERATELY_MISSING_BUCKET_01A1A316")
    );
    assert!(!f.root.path().join("executions").exists());
    assert!(f.run_flags("a", &["--no-cache"]).status.success());
    assert_eq!(f.count(), 1);
}

#[test]
fn dedicated_flux_credentials_sign_requests_without_ambient_compiler_credentials() {
    let server = mock_s3::Server::new(false);
    let f = Fixture::new(false);
    configure_mock_store(&f, &server.endpoint, 5);
    let output = native_command(&f).arg("--cache-required").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("cache saved: produce"));
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert!(request.contains("Credential=flux-key/"));
        assert!(!request.contains("compiler-key"));
        assert!(
            !request
                .to_ascii_lowercase()
                .contains("x-amz-security-token")
        );
    }
}

#[test]
fn hit_required_rejects_misses_but_still_executes_on_hits() {
    let f = Fixture::new(false);
    let output = f.run_flags("a", &["--cache-hit-required"]);
    assert!(!output.status.success());
    assert!(!f.root.path().join("executions").exists());
    f.success("a", false);
    f.clear_outputs();
    let output = f.run_flags("a", &["--cache-hit-required"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("cache hit: produce"));
    assert_eq!(f.count(), 2);
}

#[test]
fn optional_input_creation_invalidates_the_result() {
    let f = Fixture::new(false);
    let config = f.root.path().join("flux.toml");
    fs::write(
        &config,
        fs::read_to_string(&config).unwrap().replace(
            "inputs = [\"source\"]",
            "inputs = [\"source\"], optional_inputs = [\"optional.conf\"]",
        ),
    )
    .unwrap();
    f.success("a", false);
    assert!(f.success("a", false).contains("cache hit: produce"));
    fs::write(f.root.path().join("optional.conf"), "new config").unwrap();
    assert!(f.success("a", false).contains("cache miss: produce"));
    assert_eq!(f.count(), 3);
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn configure_mock_store(f: &Fixture, endpoint: &str, timeout: u64) {
    let config = f.root.path().join("flux.toml");
    fs::write(
        &config,
        format!(
            "{}\n[cache.s3]\nbucket='fixture'\nendpoint={endpoint:?}\ntimeout_seconds={timeout}\n",
            fs::read_to_string(&config).unwrap()
        ),
    )
    .unwrap();
}

fn native_command(f: &Fixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-flux"));
    command
        .arg("--root")
        .arg(f.root.path())
        .args(["run", "produce"])
        .env("FLUX_CACHE_PRODUCER", "1")
        .env("CACHE_TEST_MODE", "a")
        .env("AWS_ACCESS_KEY_ID", "compiler-key")
        .env("AWS_SECRET_ACCESS_KEY", "compiler-secret")
        .env("AWS_SESSION_TOKEN", "compiler-token")
        .env("AWS_PROFILE", "compiler-profile")
        .env("CARGO_FLUX_S3_ACCESS_KEY_ID", "flux-key")
        .env("CARGO_FLUX_S3_SECRET_ACCESS_KEY", "flux-secret")
        .env_remove("CARGO_FLUX_S3_SESSION_TOKEN")
        .env("CARGO_FLUX_S3_REGION", "us-east-1")
        .env("NO_PROXY", "*");
    command
}

#[test]
fn prefetched_entry_is_discarded_if_a_prerequisite_changes_the_key() {
    let f = Fixture::new(false);
    f.success("a", false);
    let config = f.root.path().join("flux.toml");
    let text = fs::read_to_string(&config).unwrap().replace(
        "[tasks.produce]\n",
        "[tasks.produce]\ndepends_on=['prepare']\n",
    );
    fs::write(
        config,
        format!("{text}\n[tasks.prepare]\nroot=['git','config','flux.cache-version','two']\n"),
    )
    .unwrap();
    let output = f.success("a", false);
    assert!(output.contains("cache miss: produce"), "{output}");
    assert_eq!(f.count(), 2);
}
