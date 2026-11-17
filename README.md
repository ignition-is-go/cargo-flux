# cargo-flux

Plan and run tasks across Cargo, JavaScript and uv/Python workspaces from one
`flux.toml`. Flux coordinates package order and cross-language dependencies;
Cargo, your JavaScript package manager and uv still do the builds.

- **Inspect:** discover packages and preview task order.
- **Run:** execute package tasks and commands that run once at the repo root.
- **Scope:** select a product or run only work affected by committed changes.
- **Reuse:** restore intermediate files, save build snapshots and transfer artifacts.
- **Release:** calculate versions and stamp Cargo and JavaScript manifests.

[Quick start](#quick-start) · [Task order](#control-task-order) ·
[Changed packages](#run-only-changed-work) · [Caching](#reuse-intermediate-files) ·
[Snapshots](#save-and-restore-build-snapshots) · [CI](#use-in-github-actions) ·
[Full reference](docs/reference.md) · [Contributing](CONTRIBUTING.md)

## Install

```sh
cargo binstall cargo-flux
# Or compile from crates.io:
cargo install cargo-flux --locked
```

To use the checked-out source, including features not yet released:

```sh
cargo install --path . --locked --force
```

Both `cargo flux` and `cargo-flux` invoke the CLI. Run commands from the repo root,
or add `--root /path/to/repo`.

## Quick start

Start in an existing native workspace. Flux discovers members declared in
`Cargo.toml`, `package.json` / `pnpm-workspace.yaml`, and `pyproject.toml`; it does
not scan arbitrary directories for projects. Install each project's native tools
and dependencies first.

Create **`flux.toml`** at the workspace root:

```toml
[tasks.build]
autoapply = "all"
workspace_batchable = true
cargo = ["cargo", "build"]
npm = ["npm", "run", "build"]
uv = ["uv", "build"]

[tasks.test]
autoapply = "all"
cargo = ["cargo", "test"]
npm = ["npm", "test"]
uv = ["uv", "run", "pytest"]
```

Keep the command variants your repo uses. JavaScript examples assume matching
`build` and `test` scripts; the Python test example assumes pytest is installed
in the project's uv environment. Add `pnpm`, `yarn` or `bun` variants when needed;
Flux prefers the detected package manager's variant, then `npm`, then `default`.

```sh
cargo flux graph               # Package relationships
cargo flux topo                # Packages in dependency order
cargo flux plan build          # Preview without executing
cargo flux plan build --ordered
cargo flux run build
cargo flux run test
```

`autoapply = "all"` starts with every compatible package **when none explicitly
opts into that task**. Tasks normally run from each package directory. Compatible
Cargo builds can be batched into one workspace invocation when
`workspace_batchable = true`.

The examples below are independent. Merge fields into an existing TOML table
rather than declaring the same table twice.

## Choose which packages participate

For explicit entrypoints, replace the build definition with:

```toml
# flux.toml
[tasks.build]
autoapply = "inherit"
cargo = ["cargo", "build"]
npm = ["npm", "run", "build"]
uv = ["uv", "build"]
```

Opt packages in using their native manifests:

```toml
# Cargo.toml, inside a Rust package
[package.metadata.flux]
tasks = ["build"]
```

```json
{
  "name": "@demo/web",
  "scripts": { "build": "vite build" },
  "flux": { "tasks": ["build"] }
}
```

```toml
# pyproject.toml, inside a Python package
[tool.flux]
tasks = ["build"]
```

`autoapply = "inherit"` lets dependencies reached by the plan receive the task.
The default, `"none"`, requires an explicit opt-in on every participating package.
A JavaScript script alone is not a Flux opt-in.

## Control task order

**Run another task first on the same package:**

```toml
[tasks.build]
autoapply = "all"
depends_on = ["test"]
cargo = ["cargo", "build"]

[tasks.test]
autoapply = "inherit"
cargo = ["cargo", "test"]
```

**Run a dependency in another language first:** add a bridge to the package
that consumes it. For example, a web package that needs Rust package `api`:

```json
{
  "name": "@demo/web",
  "scripts": { "build": "vite build" },
  "flux": {
    "tasks": ["build"],
    "bridges": ["cargo:api"]
  }
}
```

Use the `autoapply = "inherit"` build definition above. `cargo flux plan build`
then schedules `api:build` before `@demo/web:build`. Bridges carry the same task
name and use scoped package identities: `cargo:api`, `js:@demo/web`, `uv:codegen`.

Native dependencies are traversed to find bridges, but Flux normally lets the
native build tool handle native dependency builds. To schedule the same task on
native dependencies too—for example, publish dependencies before their consumers:

```toml
[tasks.publish]
autoapply = "inherit"
cascade = "all"
include_dev_dependencies = true
cargo = ["cargo", "publish", "--locked"]
```

Opt a release entrypoint into `publish`, then inspect `cargo flux plan publish`
before running it. See the [planning rules](docs/reference.md#planning-semantics)
for cascade behavior and [ecosystem-specific prerequisites](docs/reference.md#ecosystem_depends_on).

## Run only changed work

```sh
git fetch origin main
cargo flux affected --base origin/main
cargo flux plan test --affected origin/main
cargo flux run test --affected origin/main
```

Flux compares committed `HEAD` with the merge base of `origin/main`, then includes
affected consumers. Staged, unstaged and untracked edits are not part of this
comparison. The base and enough history to find a merge base must exist locally.

Root manifests, lockfiles and shared or unowned inputs conservatively affect the
whole workspace. Exclude files that cannot change task behavior:

```toml
[affected]
ignore = ["docs/**", "AGENTS.md"]
```

An empty affected scope succeeds without scheduling root or package tasks.

For a reusable product boundary, select packages independently of task opt-in:

```toml
[selections.product]
roots = ["cargo:api", "js:@demo/web"]
dependencies = true
bridges = true
```

```sh
cargo flux select product
cargo flux plan build --selection product
cargo flux run build --selection product --affected origin/main
```

A task can set `selection = "product"` as its default. Selection narrows the
package set; it does not opt those packages into tasks.

## Run commands once at the repo root

Use `root` for setup or work that belongs to the whole repo:

```toml
[tasks.prepare]
root = ["npm", "ci"]

[tasks.build]
depends_on = ["prepare"]
autoapply = "all"
cargo = ["cargo", "build"]
npm = ["npm", "run", "build"]
```

`cargo flux run build` runs `prepare` once before package builds. A task can also
define its own `root` command alongside package variants. Root commands depend
only on other root commands.

For multiple steps and a captured value:

```toml
[tasks.calculate-version]
root = ["cargo", "flux", "version"]
outputs = { version = "stdout" }

[tasks.prepare-release]
depends_on = ["calculate-version"]
when = { output = "calculate-version.version", nonempty = true }
root_steps = [
  ["cargo", "flux", "stamp", "${calculate-version.version}"],
  ["cargo", "update", "--workspace"],
]
```

```sh
cargo flux run prepare-release --report release-report.json
```

Steps stop on failure. Substituted values remain individual arguments. Empty
version output skips the dependent release task successfully. Dynamic outputs
and conditions currently require root-only tasks. Here, `outputs` means captured
stdout values; cached files are declared separately under `cache.paths`.

## Reuse intermediate files

A root-only task can cache files between runs. This example assumes a Node
project with an `npm run build` script that writes `dist/`:

```toml
[tasks.bundle]
root = ["npm", "run", "build"]

[tasks.bundle.cache]
inputs = ["src/**/*", "package.json", "package-lock.json"]
paths = ["dist"]
env = ["NODE_ENV"]
tools = [["node", "--version"], ["npm", "--version"]]
```

```sh
cargo flux run bundle
cargo flux run bundle --no-cache
cargo flux run bundle --cache-required
cargo flux run bundle --cache-hit-required
```

A hit restores the files and **still runs the command**. Flux caches intermediates,
not a previous validation result. Declare every input, environment value and tool
version that affects the result. Use `optional_inputs` for files that may not exist.

The default store is `.flux/cache`; add it to `.gitignore`. Cache errors normally
fall back to execution. `--cache-required` makes errors fatal but permits misses;
`--cache-hit-required` requires a hit before execution. Both conflict with
`--no-cache`. See [cache details](docs/reference.md#intermediate-file-caching).

## Save and restore build snapshots

A snapshot is an explicit file-transfer contract. Declare it in **`flux.toml`**:

```toml
[snapshots.rust-build]
plugin = "cargo"
paths = ["target"]

[snapshots.rust-build.options]
manifest_paths = ["Cargo.toml"]
```

In a Cargo workspace with the build task from the quick start:

```sh
KEY=$(cargo flux storage key --plugin cargo --scope build)
cargo flux storage restore --snapshot rust-build --key "$KEY"
cargo flux storage sources --restore
cargo flux run build
cargo flux storage sources
cargo flux storage save --snapshot rust-build --key "$KEY"
```

The Cargo plugin excludes obsolete compiler units and incremental sessions while
preserving reusable dependencies and feature variants. Source timestamp recording
preserves unchanged tracked inputs across checkouts; the repo must be in Git.
Cargo always decides freshness when the build runs.

Paths are relative to the workspace root. For shared targets, list every
contributing workspace in `manifest_paths`. Saving selects archive contents and
leaves local files in place; it is not a cleanup step. Unknown compiler units are
retained conservatively, so selection does not impose a total size limit.

Restore needs only the declared paths and archive, without loading the plugin's
build metadata. Set `FALLBACK_KEY` to another compatible snapshot key to try a
fallback while preserving spare disk capacity:

```sh
cargo flux storage restore --snapshot rust-build \
  --key "$KEY" --key "$FALLBACK_KEY" --reserve-bytes 1073741824
```

Optional misses or capacity rejections allow the build to continue; `--required` makes the restore mandatory.
Capacity checks cannot reserve space against other writers.

For one-off saves, use inline flags instead of a named snapshot:

```sh
cargo flux storage save --key "$KEY" --path target --plugin cargo \
  --plugin-option manifest-path=Cargo.toml
```

`--snapshot` cannot be combined with inline path, artifact or plugin flags.
The generic storage layer owns validation, archives and transport; the Cargo
plugin owns compiler identities and selection. [Snapshot details](docs/reference.md#intermediate-file-caching).

## Transfer finished artifacts

Declare a generic snapshot without a plugin to preserve all declared files and
empty directories:

```toml
[snapshots.assets]
paths = ["dist"]
```

After your build has created `dist/`, transfer it between jobs or machines using
the same configured store:

```sh
# Producer: use an immutable identity for this build and platform.
KEY="artifacts/$(git rev-parse HEAD)/linux"
cargo flux storage save --snapshot assets --key "$KEY"

# Consumer: use that same key and snapshot paths.
cargo flux storage restore --snapshot assets --key "$KEY" \
  --destination prebuilt --required
```

The consumer receives `prebuilt/dist/`. Existing `[artifacts]` lists also work
with `--artifact NAME`; explicit paths work with repeated `--path` flags.

For shared S3-compatible storage, add:

```toml
[cache.s3]
bucket = "build-cache"
endpoint = "http://localhost:9000" # Omit for AWS S3.
prefix = "flux"
timeout_seconds = 120
```

Provide `CARGO_FLUX_S3_ACCESS_KEY_ID` and `CARGO_FLUX_S3_SECRET_ACCESS_KEY` through
the environment. Optional settings are `CARGO_FLUX_S3_SESSION_TOKEN` and
`CARGO_FLUX_S3_REGION` (default `us-east-1`). For a shared local filesystem instead:

```toml
[cache]
directory = "/shared/flux-cache"
```

The same store serves task caches, snapshots and artifacts.

## Use in GitHub Actions

A complete pull-request workflow for the build/test task definitions above:

```yaml
name: Test
on: pull_request
permissions:
  contents: read
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      # Set up your language toolchains and install project dependencies here.
      - id: flux
        uses: ignition-is-go/cargo-flux@v0.11.8
        with:
          task: test
          affected: origin/${{ github.base_ref }}
```

Use an action release that supports the configuration in your repo. To try
unreleased manifest features, build the CLI from the checkout instead.

Omit `task` for setup-only use. Later shell steps should invoke
`"$CARGO_FLUX_BIN" run build` to retain the exact installed version. The action
exposes `outcome` (`completed` or `skipped`) and JSON `task-outputs`; it can also
configure S3 and sccache. [Action inputs and behavior](docs/reference.md#github-action).

## Calculate and stamp versions

```toml
[channels]
main = "production"

[channels."release/*"]
channel = "rc"
prerelease = true
```

```sh
cargo flux version                 # Calculate without changing files
cargo flux version --channel rc
cargo flux stamp 1.2.3              # Write Cargo and JavaScript versions
cargo flux stamp 1.2.3 --package api
```

Since the latest production tag, `fix:` produces a patch, `feat:` a minor, and a
breaking change a major bump. Without a release-worthy change, `version` prints
nothing and succeeds. `stamp` without a version calculates it first. Stamping
updates dependency requirements only when their targets are also selected.
See [release rules](docs/reference.md#version) and the
[release-task example](docs/reference.md#self-publishing-example).

## Reference and limits

Use `cargo flux --help` or `cargo flux COMMAND --help` for flags. The
[full reference](docs/reference.md) covers task variables, package discovery,
bridge syntax, release channels, storage compatibility and plugin boundaries.

Execution is sequential; Cargo workspace batching is currently the only batching
implementation. Cross-language ordering needs explicit bridges. Files restored
from any cache remain subject to the native tool's freshness and validation rules.
