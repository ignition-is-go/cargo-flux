use std::process::{Command, Output};

fn script() -> String {
    let action: serde_yaml::Value = serde_yaml::from_str(include_str!("../action.yml")).unwrap();
    action["runs"]["steps"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|step| step["name"].as_str() == Some("Configure optional S3 cache environment"))
        .unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn command(env_file: &std::path::Path) -> Command {
    let mut command = Command::new("bash");
    command
        .args(["-c", &script()])
        .env("GITHUB_ENV", env_file)
        .env("CACHE_MODE", "s3")
        .env("CACHE_REQUIRED", "true")
        .env("CACHE_ENDPOINT", "http://localhost:9000")
        .env("CACHE_BUCKET", "flux-test")
        .env("CACHE_ACCESS_KEY_ID", "flux-key")
        .env("CACHE_SECRET_ACCESS_KEY", "secret%25value")
        .env("CACHE_SESSION_TOKEN", "")
        .env("CACHE_REGION", "us-east-1")
        .env("AWS_ACCESS_KEY_ID", "compiler-key")
        .env("AWS_SECRET_ACCESS_KEY", "compiler-secret");
    command
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn action_exports_only_flux_names_and_masks_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("env");
    let output = command(&path).output().unwrap();
    success(&output);
    let exported = std::fs::read_to_string(path).unwrap();
    assert!(exported.contains("CARGO_FLUX_S3_BUCKET=flux-test\n"));
    assert!(exported.contains("CARGO_FLUX_S3_SECRET_ACCESS_KEY=secret%25value\n"));
    assert!(
        exported
            .lines()
            .all(|line| line.starts_with("CARGO_FLUX_S3_"))
    );
    assert!(!exported.contains("compiler-key"));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("::add-mask::flux-key"));
    assert!(stdout.contains("::add-mask::secret%2525value"));
}

#[test]
fn invalid_or_multiline_credentials_never_write_environment_entries() {
    let directory = tempfile::tempdir().unwrap();
    for value in ["", "credential\nINJECTED=bad", "credential\rINJECTED=bad"] {
        let path = directory.path().join("env");
        let output = command(&path)
            .env("CACHE_SECRET_ACCESS_KEY", value)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!path.exists());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("credential"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("INJECTED"));
    }
}

#[test]
fn setup_only_without_cache_does_not_change_the_environment() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("env");
    let output = command(&path).env("CACHE_MODE", "").output().unwrap();
    success(&output);
    assert!(!path.exists());
    assert!(
        !command(&path)
            .env("CACHE_MODE", "unknown")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        !command(&path)
            .env("CACHE_REQUIRED", "unknown")
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[cfg(unix)]
#[test]
fn compiler_credentials_are_isolated_to_the_daemon_and_read_only_is_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("sccache");
    std::fs::write(&binary, r#"#!/bin/bash
printf '%s:%s:%s:%s\n' "$1" "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY" "$SCCACHE_S3_RW_MODE" >> "$CAPTURE"
"#).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let action: serde_yaml::Value = serde_yaml::from_str(include_str!("../action.yml")).unwrap();
    let script = action["runs"]["steps"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|step| step["name"].as_str() == Some("Configure isolated compiler-cache daemon"))
        .unwrap()["run"]
        .as_str()
        .unwrap();
    let env_file = directory.path().join("env");
    let capture = directory.path().join("calls");
    let output = Command::new("bash")
        .args(["-c", script])
        .env(
            "PATH",
            format!(
                "{}:{}",
                directory.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("CAPTURE", &capture)
        .env("GITHUB_ENV", &env_file)
        .env("GITHUB_WORKSPACE", directory.path())
        .env("RUNNER_OS", "Linux")
        .env("COMPILER_CACHE", "true")
        .env("COMPILER_READ_ONLY", "true")
        .env("COMPILER_PREFIX", "compiler/v1")
        .env("CARGO_FLUX_S3_ENDPOINT", "http://localhost:9000")
        .env("CARGO_FLUX_S3_BUCKET", "flux")
        .env("CARGO_FLUX_S3_ACCESS_KEY_ID", "flux-client")
        .env("CARGO_FLUX_S3_SECRET_ACCESS_KEY", "flux-secret")
        .env("AWS_ACCESS_KEY_ID", "existing-client")
        .env("AWS_SECRET_ACCESS_KEY", "existing-secret")
        .output()
        .unwrap();
    success(&output);
    let calls = std::fs::read_to_string(capture).unwrap();
    assert!(calls.contains("--start-server:flux-client:flux-secret:READ_ONLY"));
    assert!(calls.contains("--zero-stats:existing-client:existing-secret:READ_ONLY"));
    let exported = std::fs::read_to_string(env_file).unwrap();
    assert!(!exported.contains("AWS_"));
    assert!(!exported.contains("flux-secret"));
    assert!(exported.contains("SCCACHE_S3_RW_MODE=READ_ONLY"));
    assert!(exported.contains("RUSTC_WRAPPER=sccache"));
    let socket = exported
        .lines()
        .find_map(|line| line.strip_prefix("SCCACHE_SERVER_UDS="))
        .unwrap();
    std::fs::remove_dir(std::path::Path::new(socket).parent().unwrap()).unwrap();
}
