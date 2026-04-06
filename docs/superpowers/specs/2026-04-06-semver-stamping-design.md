# Semantic Versioning and Version Stamping

Add `cargo flux version` and `cargo flux stamp` subcommands that calculate the next semantic version from git tags and conventional commits, and stamp it across all discovered workspace manifests.

## Subcommands

### `cargo flux version`

Prints the next version to stdout. Pure query, no side effects.

```
cargo flux version              # auto-detect channel from current branch
cargo flux version --channel X  # override channel explicitly
```

Exit code 0 on success, non-zero if no production tag exists or the branch isn't mapped to a channel.

### `cargo flux stamp [<version>]`

Writes a version string to every discovered workspace manifest.

- If `<version>` is provided, stamps that literal string.
- If omitted, runs the same calculation as `cargo flux version` and stamps the result.

Prints each file path it modifies to stderr.

## Channel Configuration

Channels are configured in `flux.toml` under a `[channels]` table:

```toml
[channels]
main = "production"
dev = "canary"

[channels."release/beta"]
channel = "beta"
prerelease = true

[channels."release/*"]
channel = "beta"
prerelease = true
```

- Shorthand `branch = "channel-name"` implies `prerelease = false`.
- Table form `{ channel = "...", prerelease = true }` for prerelease channels.
- Branch names support trailing `*` glob for pattern matching.
- Exact matches take priority over glob matches.
- A non-prerelease channel produces `X.Y.Z`.
- A prerelease channel produces `X.Y.Z-channel.N` where N is one more than the highest existing tag for that base version and channel.

## New Modules

### `src/version.rs`

Pure logic, no I/O.

- `Version { major, minor, patch }` — parse from tag strings like `v3.1.0` or `3.1.0`.
- `Bump { Patch, Minor, Major }` — determined from conventional commits.
- `calculate_next_version(current: Version, commits: &[String]) -> Version` — scans commit messages for `feat`, `fix`, `BREAKING CHANGE`, and `!:` patterns.
- `Version::format() -> String` — renders as `"X.Y.Z"`.

Ported directly from rship's `version.rs`.

### `src/git.rs`

Thin wrappers over `git` CLI via `std::process::Command`. Each function captures stdout.

- `get_current_branch() -> Result<String>`
- `get_latest_production_tag() -> Option<String>` — latest tag matching `vX.Y.Z` exactly (no prerelease suffix), sorted by version.
- `get_commits_since(tag: Option<&str>) -> Vec<String>` — commit subjects since a tag.
- `get_existing_prerelease_count(base_version: &str, channel: &str) -> u32` — highest N in `vX.Y.Z-channel.N` tags.

### `src/channels.rs`

- `ChannelConfig { channel: String, prerelease: bool }` — parsed from `flux.toml`.
- `parse_channels(config: &toml::Value) -> HashMap<String, ChannelConfig>` — handles both shorthand and table forms.
- `resolve_channel(branch: &str, channels: &HashMap<String, ChannelConfig>) -> Option<ChannelConfig>` — exact match first, then glob patterns.

### `src/stamp.rs`

Uses the existing `manifest::discover_workspace` to find packages.

- `stamp_all(root: &Path, version: &str) -> Result<()>` — walks discovered packages and rewrites version fields.
- For `Cargo.toml`: updates `[package] version` and intra-workspace path dependency versions.
- For `package.json`: updates the top-level `"version"` field.
- Uses `toml_edit` for Cargo.toml to preserve formatting.
- Uses `serde_json` for package.json (already a dependency).

## Version Calculation Flow

1. Find the latest production tag (stable `vX.Y.Z`, no prerelease suffix) via `git tag -l`.
2. Collect all commit subjects since that tag.
3. Parse each commit with conventional commit patterns to determine the highest bump level (patch < minor < major).
4. Apply bump to get the base version.
5. Resolve the current branch to a channel config.
6. If the channel is prerelease, find the highest existing `vX.Y.Z-channel.N` tag and output `X.Y.Z-channel.(N+1)`.
7. If the channel is production, output `X.Y.Z`.

## Stamping Flow

1. Call `discover_workspace(&root)` to get all packages.
2. For each package, based on its ecosystem:
   - **Cargo**: parse with `toml_edit`, set `package.version`, update any `[dependencies]` entries that are intra-workspace path deps with a version field.
   - **Npm/package.json**: parse with `serde_json`, set `"version"`, write back.
3. Print each modified file path to stderr.

## What Stays Out

These are user responsibilities via flux tasks, not built into the tool:

- Git commit, tag, push
- GitHub/GitLab release creation
- Release notes generation
- Code formatting after stamp
- Lock file updates (`cargo update --workspace`, `bun install --lockfile-only`)

Example flux task composition:

```toml
[tasks.release]
shell = """
  VERSION=$(cargo flux version)
  cargo flux stamp "$VERSION"
  git add -A && git commit -m "chore(release): $VERSION"
  git tag "v$VERSION" -m "Release $VERSION"
  git push origin HEAD "v$VERSION"
"""
```

## New Dependencies

- `regex` — conventional commit parsing, tag matching.
- `toml_edit` — format-preserving Cargo.toml edits for stamping.

`glob` is already present but may not be needed; simple `ends_with` matching on `*`-suffix patterns is sufficient for branch glob resolution.

## Testing

- **`version.rs`**: Unit tests for parse and bump calculation. Port rship's existing tests plus edge cases (no commits, mixed commit types).
- **`channels.rs`**: Unit tests for config parsing (shorthand, table, mixed), branch resolution (exact, glob, no match), and priority (exact over glob).
- **`stamp.rs`**: Integration tests with temp directories containing sample Cargo.toml and package.json files. Verify version field is updated and formatting is preserved.
- **`git.rs`**: Integration tests with temp git repos, creating tags, verifying query functions return correct results.
