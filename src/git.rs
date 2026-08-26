//! Git operations for version calculation

use anyhow::{Context, Result, bail};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Execute a git command and capture trimmed stdout
fn exec(cmd: &str) -> Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .context(format!("failed to execute: {}", cmd))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("command failed: {}\n{}", cmd, stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Return files changed on `HEAD` since its merge base with `base`.
///
/// Rename detection is disabled deliberately so both sides of a rename are
/// reported. That lets package moves and deletions invalidate the old owner as
/// well as the new one.
pub fn get_changed_files(root: &Path, base: &str) -> Result<Vec<PathBuf>> {
    let base_commit = git_output(
        root,
        [
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{base}^{{commit}}"),
        ],
    )
    .with_context(|| {
        format!(
            "could not resolve base ref `{base}` as a commit; fetch the base branch and ensure checkout history is deep enough"
        )
    })?;
    let range = format!("{base_commit}...HEAD");
    let output = Command::new("git")
        .args([
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--relative",
            &range,
            "--",
        ])
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to compare HEAD with base ref `{base}`"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("failed to compare HEAD with base ref `{base}`: {stderr}");
    }

    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(PathBuf::from)
                .context("git diff returned a path that is not valid UTF-8")
        })
        .collect()
}

fn git_output<const N: usize>(root: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .context("failed to execute git")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("git command failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Get the current git branch name
pub fn get_current_branch() -> Result<String> {
    exec("git rev-parse --abbrev-ref HEAD")
}

/// Get the latest stable production tag (vX.Y.Z with no prerelease suffix)
pub fn get_latest_production_tag() -> Option<String> {
    let output = exec("git tag -l 'v*' --sort=-v:refname").ok()?;
    let stable_re = Regex::new(r"^v\d+\.\d+\.\d+$").unwrap();

    output
        .lines()
        .find(|tag| stable_re.is_match(tag))
        .map(|s| s.to_string())
}

/// Get all commit subjects since a given tag (or all commits if None)
pub fn get_commits_since(tag: Option<&str>) -> Vec<String> {
    let range = match tag {
        Some(t) => format!("{}..HEAD", t),
        None => "HEAD".to_string(),
    };

    exec(&format!("git log {} --pretty=format:%s", range))
        .unwrap_or_default()
        .lines()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Get the highest prerelease number for a base version and channel.
pub fn get_existing_prerelease_count(base_version: &str, channel: &str) -> u32 {
    let pattern = format!("v{}-{}.*", base_version, channel);
    let output = exec(&format!("git tag -l '{}'", pattern)).unwrap_or_default();

    let re = Regex::new(&format!(r"-{}\.(\d+)$", regex::escape(channel))).unwrap();

    output
        .lines()
        .filter_map(|tag| {
            re.captures(tag)
                .and_then(|caps| caps.get(1))
                .and_then(|m| m.as_str().parse::<u32>().ok())
        })
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn temp_git_repo(prefix: &str) -> PathBuf {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis();
        let path = std::env::temp_dir().join(format!("cargo-flux-git-{prefix}-{millis}"));
        fs::create_dir_all(&path).expect("create temp dir");

        let run = |cmd: &str| {
            Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(&path)
                .output()
                .expect("git command");
        };

        run("git init");
        run("git config user.email 'test@test.com'");
        run("git config user.name 'Test'");
        run("git commit --allow-empty -m 'init'");
        path
    }

    fn git(repo: &std::path::Path, cmd: &str) -> String {
        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(repo)
            .output()
            .expect("git command");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn finds_latest_production_tag() {
        let repo = temp_git_repo("prod-tag");
        git(&repo, "git tag v1.0.0");
        git(&repo, "git commit --allow-empty -m 'bump'");
        git(&repo, "git tag v1.1.0");
        git(&repo, "git tag v1.2.0-beta.1");

        let _guard = SetCurrentDir::new(&repo);
        let tag = get_latest_production_tag();
        assert_eq!(tag.as_deref(), Some("v1.1.0"));
    }

    #[test]
    fn returns_none_when_no_production_tags() {
        let repo = temp_git_repo("no-tags");
        let _guard = SetCurrentDir::new(&repo);
        let tag = get_latest_production_tag();
        assert_eq!(tag, None);
    }

    #[test]
    fn counts_commits_since_tag() {
        let repo = temp_git_repo("commits-since");
        git(&repo, "git tag v1.0.0");
        git(&repo, "git commit --allow-empty -m 'fix: first'");
        git(&repo, "git commit --allow-empty -m 'feat: second'");

        let _guard = SetCurrentDir::new(&repo);
        let commits = get_commits_since(Some("v1.0.0"));
        assert_eq!(commits.len(), 2);
        assert!(commits.iter().any(|c| c.contains("fix: first")));
        assert!(commits.iter().any(|c| c.contains("feat: second")));
    }

    #[test]
    fn finds_highest_prerelease_count() {
        let repo = temp_git_repo("prerelease-count");
        git(&repo, "git tag v1.0.0-beta.1");
        git(&repo, "git commit --allow-empty -m 'bump'");
        git(&repo, "git tag v1.0.0-beta.2");
        git(&repo, "git commit --allow-empty -m 'bump'");
        git(&repo, "git tag v1.0.0-beta.5");

        let _guard = SetCurrentDir::new(&repo);
        let count = get_existing_prerelease_count("1.0.0", "beta");
        assert_eq!(count, 5);
    }

    #[test]
    fn prerelease_count_is_zero_when_no_tags() {
        let repo = temp_git_repo("no-prerelease");
        let _guard = SetCurrentDir::new(&repo);
        let count = get_existing_prerelease_count("1.0.0", "beta");
        assert_eq!(count, 0);
    }

    #[test]
    fn changed_files_use_merge_base_and_exclude_base_only_commits() {
        let repo = temp_git_repo("changed-merge-base");
        git(&repo, "git branch base");
        git(&repo, "git checkout -b feature");
        fs::write(repo.join("feature.txt"), "feature").expect("write feature file");
        git(
            &repo,
            "git add feature.txt && git commit -m 'feature change'",
        );
        git(&repo, "git checkout base");
        fs::write(repo.join("base.txt"), "base").expect("write base file");
        git(&repo, "git add base.txt && git commit -m 'base change'");
        git(&repo, "git checkout feature");

        let changed = get_changed_files(&repo, "base").expect("diff feature from base");

        assert_eq!(changed, vec![PathBuf::from("feature.txt")]);
    }

    #[test]
    fn changed_files_preserve_spaces_and_newlines() {
        let repo = temp_git_repo("changed-special-paths");
        git(&repo, "git branch base");
        git(&repo, "git checkout -b feature");
        fs::write(repo.join("file with spaces.txt"), "spaces").expect("write spaced file");
        fs::write(repo.join("file\nwith-newline.txt"), "newline").expect("write newline file");
        git(&repo, "git add . && git commit -m 'special paths'");

        let changed = get_changed_files(&repo, "base").expect("diff special paths");

        assert!(changed.contains(&PathBuf::from("file with spaces.txt")));
        assert!(changed.contains(&PathBuf::from("file\nwith-newline.txt")));
    }

    #[test]
    fn changed_files_fail_for_missing_base() {
        let repo = temp_git_repo("changed-missing-base");
        let error =
            get_changed_files(&repo, "definitely-missing").expect_err("missing base must fail");

        assert!(error.to_string().contains("could not resolve base ref"));
    }

    struct SetCurrentDir {
        previous: PathBuf,
        _lock: MutexGuard<'static, ()>,
    }

    impl SetCurrentDir {
        fn new(path: &std::path::Path) -> Self {
            let lock = CWD_LOCK.lock().expect("lock current directory");
            let previous = std::env::current_dir().expect("get cwd");
            std::env::set_current_dir(path).expect("set cwd");
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for SetCurrentDir {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous);
        }
    }
}
