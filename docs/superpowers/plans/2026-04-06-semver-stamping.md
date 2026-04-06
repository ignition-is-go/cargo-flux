# Semver & Version Stamping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `cargo flux version` and `cargo flux stamp` subcommands that calculate the next semantic version from git tags + conventional commits, and stamp it across all workspace manifests.

**Architecture:** Four new modules (`version`, `git`, `channels`, `stamp`) plus CLI and config integration. `version` and `channels` are pure logic; `git` wraps CLI calls; `stamp` uses existing workspace discovery. The `FluxConfig` struct in `tasks.rs` gains an optional `channels` field.

**Tech Stack:** Rust, regex, toml_edit (new dep), existing toml/serde_json/clap deps.

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/version.rs` (create) | `Version` struct, parsing, `Bump` enum, `calculate_next_version` — pure logic, no I/O |
| `src/channels.rs` (create) | `ChannelConfig` struct, parse from TOML value, resolve branch to channel with glob matching |
| `src/git.rs` (create) | `get_current_branch`, `get_latest_production_tag`, `get_commits_since`, `get_existing_prerelease_count` — thin git CLI wrappers |
| `src/stamp.rs` (create) | `stamp_all` — walks discovered packages, rewrites version in Cargo.toml (toml_edit) and package.json (serde_json) |
| `src/cli.rs` (modify) | Add `Version` and `Stamp` variants to `Command` enum |
| `src/tasks.rs` (modify) | Add `channels` field to `FluxConfig` |
| `src/main.rs` (modify) | Wire up new subcommands |
| `Cargo.toml` (modify) | Add `regex` and `toml_edit` dependencies |

---

### Task 1: Add dependencies

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add regex and toml_edit to Cargo.toml**

```toml
regex = "1"
toml_edit = "0.22"
```

Add these two lines to the `[dependencies]` section, after the existing `serde_yaml` line.

- [ ] **Step 2: Run cargo check to verify deps resolve**

Run: `cargo check`
Expected: compiles successfully (new deps unused but resolved)

- [ ] **Step 3: Commit**

```
git add Cargo.toml Cargo.lock
git commit -m "chore: add regex and toml_edit dependencies"
```

---

### Task 2: Version module — parsing and bump calculation

**Files:**
- Create: `src/version.rs`
- Modify: `src/main.rs` (add `mod version;`)

- [ ] **Step 1: Write failing tests for Version parsing and bump calculation**

Create `src/version.rs` with the test module and a minimal struct that won't pass yet:

```rust
//! Semantic version parsing and calculation

use regex::Regex;

/// Represents a semantic version (major.minor.patch)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

/// Version bump type determined from conventional commits
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

impl Version {
    /// Parse a version from a tag string (e.g., "v3.1.0" or "3.1.0")
    pub fn parse(_tag: &str) -> Self {
        todo!()
    }

    /// Format as a version string without 'v' prefix
    pub fn format(&self) -> String {
        todo!()
    }
}

/// Calculate the next version based on conventional commit messages
pub fn calculate_next_version(_current: Version, _commits: &[String]) -> Version {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_with_v_prefix() {
        let v = Version::parse("v3.1.0");
        assert_eq!(v, Version { major: 3, minor: 1, patch: 0 });
    }

    #[test]
    fn parses_version_without_prefix() {
        let v = Version::parse("3.1.0");
        assert_eq!(v, Version { major: 3, minor: 1, patch: 0 });
    }

    #[test]
    fn parses_version_with_prerelease_suffix() {
        let v = Version::parse("v3.1.0-beta.1");
        assert_eq!(v, Version { major: 3, minor: 1, patch: 0 });
    }

    #[test]
    fn formats_version_as_string() {
        let v = Version { major: 3, minor: 1, patch: 0 };
        assert_eq!(v.format(), "3.1.0");
    }

    #[test]
    fn calculates_patch_bump_for_fix_commits() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["fix: something".to_string()];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 3, minor: 0, patch: 1 });
    }

    #[test]
    fn calculates_minor_bump_for_feat_commits() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["feat: new feature".to_string()];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 3, minor: 1, patch: 0 });
    }

    #[test]
    fn calculates_major_bump_for_breaking_bang() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["feat!: breaking change".to_string()];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 4, minor: 0, patch: 0 });
    }

    #[test]
    fn calculates_major_bump_for_breaking_change_footer() {
        let current = Version::parse("v3.0.0");
        let commits = vec!["fix: something\n\nBREAKING CHANGE: removed old API".to_string()];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 4, minor: 0, patch: 0 });
    }

    #[test]
    fn highest_bump_wins_across_mixed_commits() {
        let current = Version::parse("v1.0.0");
        let commits = vec![
            "fix: patch thing".to_string(),
            "feat: minor thing".to_string(),
            "chore: no bump".to_string(),
        ];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 1, minor: 1, patch: 0 });
    }

    #[test]
    fn defaults_to_patch_for_non_conventional_commits() {
        let current = Version::parse("v1.0.0");
        let commits = vec!["update readme".to_string()];
        assert_eq!(calculate_next_version(current, &commits), Version { major: 1, minor: 0, patch: 1 });
    }
}
```

- [ ] **Step 2: Add `mod version;` to main.rs**

Add `mod version;` after the existing `mod tasks;` line in `src/main.rs`.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib version`
Expected: FAIL — all tests panic with `todo!()`

- [ ] **Step 4: Implement Version::parse, Version::format, and calculate_next_version**

Replace the three `todo!()` bodies:

```rust
impl Version {
    /// Parse a version from a tag string (e.g., "v3.1.0" or "3.1.0")
    pub fn parse(tag: &str) -> Self {
        let re = Regex::new(r"^v?(\d+)\.(\d+)\.(\d+)").unwrap();
        if let Some(caps) = re.captures(tag) {
            Version {
                major: caps[1].parse().unwrap_or(0),
                minor: caps[2].parse().unwrap_or(0),
                patch: caps[3].parse().unwrap_or(0),
            }
        } else {
            Version {
                major: 0,
                minor: 0,
                patch: 0,
            }
        }
    }

    /// Format as a version string without 'v' prefix
    pub fn format(&self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Calculate the next version based on conventional commit messages
pub fn calculate_next_version(current: Version, commits: &[String]) -> Version {
    let mut bump = Bump::Patch;

    let feat_re = Regex::new(r"^feat(\([^)]*\))?!?:").unwrap();
    let breaking_re = Regex::new(r"^[a-z]+(\([^)]*\))?!:").unwrap();

    for msg in commits {
        if msg.contains("BREAKING CHANGE") || breaking_re.is_match(msg) {
            bump = Bump::Major;
            break;
        }
        if feat_re.is_match(msg) && bump < Bump::Minor {
            bump = Bump::Minor;
        }
    }

    match bump {
        Bump::Major => Version {
            major: current.major + 1,
            minor: 0,
            patch: 0,
        },
        Bump::Minor => Version {
            major: current.major,
            minor: current.minor + 1,
            patch: 0,
        },
        Bump::Patch => Version {
            major: current.major,
            minor: current.minor,
            patch: current.patch + 1,
        },
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib version`
Expected: all 10 tests PASS

- [ ] **Step 6: Commit**

```
git add src/version.rs src/main.rs
git commit -m "feat: add version module with semver parsing and bump calculation"
```

---

### Task 3: Channels module — config parsing and branch resolution

**Files:**
- Create: `src/channels.rs`
- Modify: `src/main.rs` (add `mod channels;`)
- Modify: `src/tasks.rs` (add `channels` field to `FluxConfig`)

- [ ] **Step 1: Write failing tests**

Create `src/channels.rs`:

```rust
//! Channel configuration and branch resolution

use std::collections::HashMap;

/// Configuration for a release channel
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelConfig {
    pub channel: String,
    pub prerelease: bool,
}

/// Parse `[channels]` table from a TOML value.
///
/// Supports two forms:
/// - Shorthand: `main = "production"` (prerelease = false)
/// - Table: `"release/beta" = { channel = "beta", prerelease = true }`
pub fn parse_channels(table: &toml::Value) -> HashMap<String, ChannelConfig> {
    todo!()
}

/// Resolve a git branch name to a channel config.
///
/// Exact matches take priority over glob patterns.
/// Glob patterns support trailing `*` only (e.g., `release/*`).
pub fn resolve_channel(
    branch: &str,
    channels: &HashMap<String, ChannelConfig>,
) -> Option<ChannelConfig> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> toml::Value {
        toml::from_str(
            r#"
main = "production"
dev = "canary"

[channels."release/beta"]
channel = "beta"
prerelease = true

[channels."release/*"]
channel = "rc"
prerelease = true
"#,
        )
        .unwrap()
    }

    // parse_channels operates on the inner [channels] table, so we need to
    // extract it. But our function takes a &toml::Value that IS the table.
    // Let's build the table directly for clarity.

    fn sample_channels_table() -> toml::Value {
        toml::from_str(
            r#"
main = "production"
dev = "canary"
"release/beta" = { channel = "beta", prerelease = true }
"release/*" = { channel = "rc", prerelease = true }
"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_shorthand_channel() {
        let channels = parse_channels(&sample_channels_table());
        assert_eq!(
            channels.get("main"),
            Some(&ChannelConfig {
                channel: "production".to_string(),
                prerelease: false,
            })
        );
    }

    #[test]
    fn parses_table_channel() {
        let channels = parse_channels(&sample_channels_table());
        assert_eq!(
            channels.get("release/beta"),
            Some(&ChannelConfig {
                channel: "beta".to_string(),
                prerelease: true,
            })
        );
    }

    #[test]
    fn resolves_exact_match() {
        let channels = parse_channels(&sample_channels_table());
        let config = resolve_channel("main", &channels);
        assert_eq!(
            config,
            Some(ChannelConfig {
                channel: "production".to_string(),
                prerelease: false,
            })
        );
    }

    #[test]
    fn resolves_glob_match() {
        let channels = parse_channels(&sample_channels_table());
        let config = resolve_channel("release/foo", &channels);
        assert_eq!(
            config,
            Some(ChannelConfig {
                channel: "rc".to_string(),
                prerelease: true,
            })
        );
    }

    #[test]
    fn exact_match_takes_priority_over_glob() {
        let channels = parse_channels(&sample_channels_table());
        let config = resolve_channel("release/beta", &channels);
        assert_eq!(
            config,
            Some(ChannelConfig {
                channel: "beta".to_string(),
                prerelease: true,
            })
        );
    }

    #[test]
    fn returns_none_for_unmapped_branch() {
        let channels = parse_channels(&sample_channels_table());
        let config = resolve_channel("feature/xyz", &channels);
        assert_eq!(config, None);
    }

    #[test]
    fn handles_empty_channels_table() {
        let table: toml::Value = toml::from_str("").unwrap();
        let channels = parse_channels(&table);
        assert!(channels.is_empty());
    }
}
```

- [ ] **Step 2: Add `mod channels;` to main.rs**

Add `mod channels;` after the `mod cli;` line in `src/main.rs`.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib channels`
Expected: FAIL — panics with `todo!()`

- [ ] **Step 4: Implement parse_channels and resolve_channel**

Replace the two `todo!()` bodies:

```rust
pub fn parse_channels(table: &toml::Value) -> HashMap<String, ChannelConfig> {
    let mut channels = HashMap::new();
    let Some(table) = table.as_table() else {
        return channels;
    };

    for (key, value) in table {
        let config = if let Some(channel) = value.as_str() {
            ChannelConfig {
                channel: channel.to_string(),
                prerelease: false,
            }
        } else if let Some(table) = value.as_table() {
            ChannelConfig {
                channel: table
                    .get("channel")
                    .and_then(|v| v.as_str())
                    .unwrap_or(key)
                    .to_string(),
                prerelease: table
                    .get("prerelease")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            }
        } else {
            continue;
        };
        channels.insert(key.clone(), config);
    }

    channels
}

pub fn resolve_channel(
    branch: &str,
    channels: &HashMap<String, ChannelConfig>,
) -> Option<ChannelConfig> {
    // Exact match first
    if let Some(config) = channels.get(branch) {
        return Some(config.clone());
    }

    // Glob match (trailing * only)
    for (pattern, config) in channels {
        if let Some(prefix) = pattern.strip_suffix('*') {
            if branch.starts_with(prefix) {
                return Some(config.clone());
            }
        }
    }

    None
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib channels`
Expected: all 7 tests PASS

- [ ] **Step 6: Add channels field to FluxConfig in tasks.rs**

In `src/tasks.rs`, modify the `FluxConfig` struct (around line 291) to add a `channels` field:

```rust
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FluxConfig {
    tasks: Option<BTreeMap<String, TaskDefinition>>,
    channels: Option<toml::Value>,
}
```

Also add a public accessor so `main.rs` can load channels:

```rust
impl TaskRegistry {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("flux.toml");
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read task config {}", path.display()))?;
        let config = toml::from_str::<FluxConfig>(&content)
            .with_context(|| format!("failed to parse task config {}", path.display()))?;
        Ok(Self {
            tasks: config.tasks.unwrap_or_default(),
            channels: config.channels,
        })
    }

    pub fn channels(&self) -> Option<&toml::Value> {
        self.channels.as_ref()
    }

    // ... existing methods unchanged ...
}
```

And add the field to the `TaskRegistry` struct:

```rust
#[derive(Debug)]
pub struct TaskRegistry {
    tasks: BTreeMap<String, TaskDefinition>,
    channels: Option<toml::Value>,
}
```

- [ ] **Step 7: Run all tests to verify nothing broke**

Run: `cargo test`
Expected: all tests PASS

- [ ] **Step 8: Commit**

```
git add src/channels.rs src/main.rs src/tasks.rs
git commit -m "feat: add channels module with config parsing and branch resolution"
```

---

### Task 4: Git module — tag and commit query wrappers

**Files:**
- Create: `src/git.rs`
- Modify: `src/main.rs` (add `mod git;`)

- [ ] **Step 1: Create src/git.rs with functions and integration tests**

```rust
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
/// E.g., for base "1.2.0" and channel "beta", finds the max N in v1.2.0-beta.N tags.
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

        // Initialize a git repo with an initial commit
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

        // We need to run from within the repo
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

    /// RAII guard that changes cwd and restores it on drop.
    /// Needed because git functions use the process cwd.
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
```

- [ ] **Step 2: Add `mod git;` to main.rs**

Add `mod git;` after the `mod graph;` line in `src/main.rs`.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib git`
Expected: all 5 tests PASS

- [ ] **Step 4: Commit**

```
git add src/git.rs src/main.rs
git commit -m "feat: add git module with tag and commit query wrappers"
```

---

### Task 5: Stamp module — rewrite version in workspace manifests

**Files:**
- Create: `src/stamp.rs`
- Modify: `src/main.rs` (add `mod stamp;`)

- [ ] **Step 1: Write failing tests**

Create `src/stamp.rs`:

```rust
//! Version stamping for workspace manifests

use anyhow::{Context, Result};
use std::path::Path;

use crate::manifest::{Ecosystem, Package};

/// Stamp a version string into all discovered workspace packages.
/// Returns the list of files that were modified.
pub fn stamp_all(packages: &[Package], version: &str) -> Result<Vec<String>> {
    todo!()
}

/// Stamp version into a Cargo.toml file, preserving formatting.
/// Updates [package] version and intra-workspace path dep versions.
fn stamp_cargo_toml(path: &Path, version: &str) -> Result<bool> {
    todo!()
}

/// Stamp version into a package.json file.
fn stamp_package_json(path: &Path, version: &str) -> Result<bool> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Ecosystem, Package, PackageId};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn stamps_cargo_toml_package_version() {
        let root = temp_dir("stamp-cargo");
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "my-crate"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();

        let modified = stamp_cargo_toml(&root.join("Cargo.toml"), "2.0.0").unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(content.contains("version = \"2.0.0\""));
        // Ensure other fields preserved
        assert!(content.contains("name = \"my-crate\""));
        assert!(content.contains("edition = \"2024\""));
    }

    #[test]
    fn stamps_cargo_toml_workspace_dep_versions() {
        let root = temp_dir("stamp-cargo-deps");
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "app"
version = "0.1.0"

[dependencies]
shared = { version = "0.1.0", path = "../shared" }
serde = "1"
"#,
        )
        .unwrap();

        let modified = stamp_cargo_toml(&root.join("Cargo.toml"), "2.0.0").unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(content.contains("[package]\nname = \"app\"\nversion = \"2.0.0\""));
        assert!(content.contains("shared = { version = \"2.0.0\", path = \"../shared\" }"));
        // External deps unchanged
        assert!(content.contains("serde = \"1\""));
    }

    #[test]
    fn stamps_package_json_version() {
        let root = temp_dir("stamp-pkg-json");
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "my-app",
  "version": "0.1.0",
  "dependencies": {}
}"#,
        )
        .unwrap();

        let modified = stamp_package_json(&root.join("package.json"), "2.0.0").unwrap();
        assert!(modified);

        let content = fs::read_to_string(root.join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["version"], "2.0.0");
        assert_eq!(parsed["name"], "my-app");
    }

    #[test]
    fn stamp_all_updates_mixed_ecosystem_packages() {
        let root = temp_dir("stamp-all-mixed");
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "rust-app"
version = "0.1.0"
"#,
        )
        .unwrap();
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "web-app",
  "version": "0.1.0"
}"#,
        )
        .unwrap();

        let packages = vec![
            Package {
                id: PackageId::new(Ecosystem::Cargo, "rust-app"),
                name: "rust-app".to_string(),
                ecosystem: Ecosystem::Cargo,
                manifest_path: root.join("Cargo.toml"),
                js_package_manager: None,
                task_opt_ins: BTreeMap::new(),
                bridged_dependencies: vec![],
                internal_dependencies: vec![],
            },
            Package {
                id: PackageId::new(Ecosystem::Js, "web-app"),
                name: "web-app".to_string(),
                ecosystem: Ecosystem::Js,
                manifest_path: root.join("package.json"),
                js_package_manager: None,
                task_opt_ins: BTreeMap::new(),
                bridged_dependencies: vec![],
                internal_dependencies: vec![],
            },
        ];

        let modified = stamp_all(&packages, "3.0.0").unwrap();
        assert_eq!(modified.len(), 2);

        let cargo_content = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(cargo_content.contains("version = \"3.0.0\""));

        let pkg_content = fs::read_to_string(root.join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&pkg_content).unwrap();
        assert_eq!(parsed["version"], "3.0.0");
    }

    #[test]
    fn skips_uv_packages() {
        let root = temp_dir("stamp-skip-uv");
        fs::write(
            root.join("pyproject.toml"),
            r#"[project]
name = "py-app"
version = "0.1.0"
"#,
        )
        .unwrap();

        let packages = vec![Package {
            id: PackageId::new(Ecosystem::Uv, "py-app"),
            name: "py-app".to_string(),
            ecosystem: Ecosystem::Uv,
            manifest_path: root.join("pyproject.toml"),
            js_package_manager: None,
            task_opt_ins: BTreeMap::new(),
            bridged_dependencies: vec![],
            internal_dependencies: vec![],
        }];

        let modified = stamp_all(&packages, "3.0.0").unwrap();
        assert!(modified.is_empty());

        // pyproject.toml unchanged
        let content = fs::read_to_string(root.join("pyproject.toml")).unwrap();
        assert!(content.contains("version = \"0.1.0\""));
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_millis();
        let path = std::env::temp_dir().join(format!("cargo-flux-stamp-{prefix}-{millis}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }
}
```

- [ ] **Step 2: Add `mod stamp;` to main.rs**

Add `mod stamp;` after the existing module declarations in `src/main.rs`.

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib stamp`
Expected: FAIL — panics with `todo!()`

- [ ] **Step 4: Implement stamp_cargo_toml**

```rust
fn stamp_cargo_toml(path: &Path, version: &str) -> Result<bool> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let mut modified = false;

    // Update [package] version
    if let Some(pkg) = doc.get_mut("package").and_then(|p| p.as_table_mut()) {
        if pkg.contains_key("version") {
            pkg["version"] = toml_edit::value(version);
            modified = true;
        }
    }

    // Update intra-workspace path dependency versions
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = doc.get_mut(section).and_then(|d| d.as_table_mut()) {
            for (_key, dep) in deps.iter_mut() {
                if let Some(table) = dep.as_inline_table_mut() {
                    if table.contains_key("path") && table.contains_key("version") {
                        table.insert("version", toml_edit::value(version).into_value().unwrap());
                        modified = true;
                    }
                }
            }
        }
    }

    if modified {
        std::fs::write(path, doc.to_string())
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(modified)
}
```

- [ ] **Step 5: Implement stamp_package_json**

```rust
fn stamp_package_json(path: &Path, version: &str) -> Result<bool> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut json: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let Some(obj) = json.as_object_mut() else {
        return Ok(false);
    };

    if !obj.contains_key("version") {
        return Ok(false);
    }

    obj.insert(
        "version".to_string(),
        serde_json::Value::String(version.to_string()),
    );

    let output = serde_json::to_string_pretty(&json)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    std::fs::write(path, format!("{}\n", output))
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(true)
}
```

- [ ] **Step 6: Implement stamp_all**

```rust
pub fn stamp_all(packages: &[Package], version: &str) -> Result<Vec<String>> {
    let mut modified = Vec::new();

    for package in packages {
        let path = &package.manifest_path;
        let was_modified = match package.ecosystem {
            Ecosystem::Cargo => stamp_cargo_toml(path, version)?,
            Ecosystem::Js => stamp_package_json(path, version)?,
            Ecosystem::Uv => false, // pyproject.toml stamping not yet supported
        };
        if was_modified {
            modified.push(path.display().to_string());
        }
    }

    Ok(modified)
}
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --lib stamp`
Expected: all 5 tests PASS

- [ ] **Step 8: Commit**

```
git add src/stamp.rs src/main.rs
git commit -m "feat: add stamp module for writing versions to workspace manifests"
```

---

### Task 6: CLI integration — wire up version and stamp subcommands

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add Version and Stamp variants to Command enum in cli.rs**

Add two new variants to the `Command` enum in `src/cli.rs`:

```rust
    /// Print the next calculated semantic version.
    Version {
        /// Override the release channel instead of auto-detecting from branch.
        #[arg(long)]
        channel: Option<String>,
    },
    /// Stamp a version into all workspace manifests.
    Stamp {
        /// Version to stamp. If omitted, calculates the next version automatically.
        version: Option<String>,
    },
```

- [ ] **Step 2: Handle Version command in main.rs**

Add the `Version` match arm in the `match cli.command` block in `src/main.rs`. Add the necessary imports at the top of the function body:

```rust
        Command::Version { channel } => {
            let tasks = TaskRegistry::load(&root)?;
            let channels_table = tasks.channels();

            let branch = if channel.is_some() {
                // Channel override doesn't need branch detection
                String::new()
            } else {
                git::get_current_branch()?
            };

            let channel_config = if let Some(override_channel) = channel {
                channels::ChannelConfig {
                    channel: override_channel.clone(),
                    prerelease: override_channel != "production",
                }
            } else {
                let channels_map = channels_table
                    .map(|t| channels::parse_channels(t))
                    .unwrap_or_default();
                channels::resolve_channel(&branch, &channels_map)
                    .ok_or_else(|| anyhow::anyhow!(
                        "branch '{}' is not mapped to a release channel in flux.toml [channels]",
                        branch
                    ))?
            };

            let latest_tag = git::get_latest_production_tag();
            let commits = git::get_commits_since(latest_tag.as_deref());

            anyhow::ensure!(
                !commits.is_empty(),
                "no commits since last production tag"
            );

            let current = latest_tag
                .as_ref()
                .map(|t| version::Version::parse(t))
                .unwrap_or(version::Version { major: 0, minor: 0, patch: 0 });

            let next = version::calculate_next_version(current, &commits);
            let base = next.format();

            let full_version = if channel_config.prerelease {
                let count = git::get_existing_prerelease_count(&base, &channel_config.channel) + 1;
                format!("{}-{}.{}", base, channel_config.channel, count)
            } else {
                base
            };

            println!("{}", full_version);
        }
```

- [ ] **Step 3: Handle Stamp command in main.rs**

Add the `Stamp` match arm:

```rust
        Command::Stamp { version: explicit_version } => {
            let discovery = discover_workspace(&root)?;

            let version_str = match explicit_version {
                Some(v) => v,
                None => {
                    // Calculate version the same way as the Version command
                    let tasks = TaskRegistry::load(&root)?;
                    let channels_table = tasks.channels();
                    let branch = git::get_current_branch()?;
                    let channels_map = channels_table
                        .map(|t| channels::parse_channels(t))
                        .unwrap_or_default();
                    let channel_config = channels::resolve_channel(&branch, &channels_map)
                        .ok_or_else(|| anyhow::anyhow!(
                            "branch '{}' is not mapped to a release channel in flux.toml [channels]",
                            branch
                        ))?;

                    let latest_tag = git::get_latest_production_tag();
                    let commits = git::get_commits_since(latest_tag.as_deref());
                    anyhow::ensure!(!commits.is_empty(), "no commits since last production tag");

                    let current = latest_tag
                        .as_ref()
                        .map(|t| version::Version::parse(t))
                        .unwrap_or(version::Version { major: 0, minor: 0, patch: 0 });

                    let next = version::calculate_next_version(current, &commits);
                    let base = next.format();

                    if channel_config.prerelease {
                        let count = git::get_existing_prerelease_count(&base, &channel_config.channel) + 1;
                        format!("{}-{}.{}", base, channel_config.channel, count)
                    } else {
                        base
                    }
                }
            };

            let modified = stamp::stamp_all(&discovery.packages, &version_str)?;
            for path in &modified {
                eprintln!("{}", path);
            }
            println!("{}", version_str);
        }
```

- [ ] **Step 4: Run cargo check**

Run: `cargo check`
Expected: compiles successfully

- [ ] **Step 5: Run all tests**

Run: `cargo test`
Expected: all tests PASS

- [ ] **Step 6: Commit**

```
git add src/cli.rs src/main.rs
git commit -m "feat: wire up version and stamp subcommands"
```

---

### Task 7: End-to-end smoke test

**Files:**
- Modify: `src/main.rs` (add integration test)

This task verifies the full flow works by testing the CLI argument parsing for the new commands.

- [ ] **Step 1: Add CLI parsing tests for new commands**

Add these tests to the existing `#[cfg(test)] mod tests` block in `src/main.rs`:

```rust
    #[test]
    fn parses_version_command() {
        let cli = Cli::parse_from(["cargo-flux", "version"]);
        match cli.command {
            Command::Version { channel } => {
                assert!(channel.is_none());
            }
            other => panic!("expected version command, got {other:?}"),
        }
    }

    #[test]
    fn parses_version_command_with_channel_override() {
        let cli = Cli::parse_from(["cargo-flux", "version", "--channel", "beta"]);
        match cli.command {
            Command::Version { channel } => {
                assert_eq!(channel.as_deref(), Some("beta"));
            }
            other => panic!("expected version command, got {other:?}"),
        }
    }

    #[test]
    fn parses_stamp_command_with_explicit_version() {
        let cli = Cli::parse_from(["cargo-flux", "stamp", "1.2.3"]);
        match cli.command {
            Command::Stamp { version } => {
                assert_eq!(version.as_deref(), Some("1.2.3"));
            }
            other => panic!("expected stamp command, got {other:?}"),
        }
    }

    #[test]
    fn parses_stamp_command_without_version() {
        let cli = Cli::parse_from(["cargo-flux", "stamp"]);
        match cli.command {
            Command::Stamp { version } => {
                assert!(version.is_none());
            }
            other => panic!("expected stamp command, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run all tests**

Run: `cargo test`
Expected: all tests PASS

- [ ] **Step 3: Commit**

```
git add src/main.rs
git commit -m "test: add CLI parsing tests for version and stamp commands"
```
