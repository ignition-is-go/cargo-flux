//! Git operations for version calculation

use anyhow::{Context, Result, bail};
use regex::Regex;
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

    exec(&format!("git log {} --pretty=format:\"%s\"", range))
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

    struct SetCurrentDir {
        previous: PathBuf,
    }

    impl SetCurrentDir {
        fn new(path: &std::path::Path) -> Self {
            let previous = std::env::current_dir().expect("get cwd");
            std::env::set_current_dir(path).expect("set cwd");
            Self { previous }
        }
    }

    impl Drop for SetCurrentDir {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous);
        }
    }
}
