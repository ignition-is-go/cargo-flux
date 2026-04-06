# cargo-flux

`cargo-flux` is a workspace topology and task-planning tool for mixed-language repositories.

It understands:

- Cargo workspaces
- JavaScript workspaces (`package.json`, `pnpm-workspace.yaml`)
- uv/Python workspaces (`pyproject.toml`)

It does not try to replace each ecosystem's native build system. Instead, it coordinates repo-wide tasks and cross-ecosystem handoffs.

## Status

The current implementation:

- calculates next semantic version from git tags and conventional commits
- stamps versions across all workspace manifests
- discovers workspace packages
- resolves native in-workspace dependencies
- loads repo-defined logical tasks from `flux.toml`
- determines which packages participate in a task
- follows cross-ecosystem bridge dependencies
- prints task plans as Unicode trees
- executes tasks in dependency order with `run`
- can batch compatible ready Cargo tasks into a single workspace invocation when a task is marked `workspace_batchable`

Use `cargo flux plan <task>` to inspect the plan without executing anything.

## Install

Install the binary locally:

```bash
cargo install --path . --force
```

Because the binary is named `cargo-flux`, Cargo exposes it as a subcommand:

```bash
cargo flux --help
```

You can also run it directly:

```bash
cargo-flux --help
```

## Commands

```bash
cargo flux graph --root /path/to/repo
cargo flux topo --root /path/to/repo
cargo flux plan build --root /path/to/repo
cargo flux run build --root /path/to/repo
cargo flux version
cargo flux stamp
cargo flux stamp 1.2.3
```

### `graph`

Prints the discovered workspace packages as an ASCII tree of native internal dependencies.

### `topo`

Prints packages in topological order.

### `plan <task>`

Prints the planned task execution tree for the given logical task.

The plan respects:

- package opt-in
- ecosystem-specific task variants
- cross-ecosystem bridges
- bridge traversal through native dependency chains

Use `--ordered` to print the actual execution order instead of the tree view:

```bash
cargo flux plan build --ordered --root /path/to/repo
```

### `run <task>`

Executes the planned tasks in dependency order.

Flux prints a `[m/n]` progress prefix as tasks start. When an ecosystem plugin batches multiple ready tasks into one execution unit, Flux shows a range like `[3-7/20]`.

By default, each task runs from its package directory. Ecosystem plugins may override that execution strategy for specific tasks when it is safe to do so. Today the Cargo plugin uses this hook for `workspace_batchable` tasks.

### `version`

Prints the next semantic version to stdout. Pure query, no side effects.

Flux determines the version by:

1. Finding the latest production git tag (`vX.Y.Z`, no prerelease suffix)
2. Collecting all commit subjects since that tag
3. Parsing conventional commits to determine the bump level:
   - `fix:` commits produce a patch bump
   - `feat:` commits produce a minor bump
   - `feat!:`, `fix!:`, or `BREAKING CHANGE` produce a major bump
4. Resolving the current branch to a release channel via `[channels]` in `flux.toml`
5. For prerelease channels, appending `-channel.N` where N is one more than the highest existing tag

```bash
cargo flux version              # auto-detect channel from current branch
cargo flux version --channel beta  # override channel
```

### `stamp [version]`

Writes a version string into every discovered workspace manifest.

- If a version argument is provided, stamps that literal string.
- If omitted, calculates the next version the same way as `version`.

Flux updates:

- `Cargo.toml`: the `[package] version` field and any intra-workspace path dependency versions
- `package.json`: the top-level `"version"` field

Prints each modified file path to stderr and the stamped version to stdout.

```bash
cargo flux stamp            # calculate and stamp
cargo flux stamp 2.0.0      # stamp an explicit version
```

## Release Channels

Release channels map git branches to version strategies. They are configured in `flux.toml`:

```toml
[channels]
main = "production"
dev = "canary"

[channels."release/beta"]
channel = "beta"
prerelease = true

[channels."release/*"]
channel = "rc"
prerelease = true
```

Shorthand `branch = "channel-name"` implies `prerelease = false` and produces stable versions like `1.2.0`.

Table form `{ channel = "...", prerelease = true }` produces prerelease versions like `1.2.0-beta.3`.

Branch names support trailing `*` for glob matching. Exact matches take priority over globs.

### Self-publishing example

Combine `version`, `stamp`, and a release task to create a self-publishing workflow:

```toml
[channels]
main = "production"

[tasks.release]
cargo = "VERSION=$(cargo flux version) && cargo flux stamp \"$VERSION\" && cargo update --workspace && cargo fmt --all && git add -A && git commit -m \"chore(release): $VERSION\" && git tag \"v$VERSION\" -m \"Release $VERSION\" && git push origin HEAD \"v$VERSION\" && cargo publish"
```

Then run:

```bash
cargo flux run release
```

## Core Model

`cargo-flux` has four separate concepts:

1. Workspaces
2. Logical tasks
3. Bridges
4. Task dependencies

### Workspaces

Each ecosystem plugin discovers the packages that belong to that ecosystem's workspace.

### Logical tasks

Logical tasks are repo-level names like `build`, `test`, `lint`, or `generate`.

They are defined once in `flux.toml`, with different command variants per ecosystem.

### Bridges

Bridges are package-level declarations that tell Flux when work needs to cross ecosystem boundaries.

Example:

- a TypeScript package may need a Rust crate to run `build` first
- a Rust crate may need a Python package to run `generate` first

Bridges are package relationships, not task-specific edges. Flux carries the current task name across the bridge.

### Task dependencies

Task dependencies are repo-level relationships between logical tasks.

Example:

- `build` may depend on `test`
- `publish` may depend on `build`

These dependencies apply per package. Flux runs the dependency task on the same package before the dependent task.

## How Workspace Discovery Works

Flux does not recursively crawl every manifest it can find. It respects the native workspace declarations for each ecosystem.

### Cargo

Flux reads the root `Cargo.toml` and uses:

- `[workspace].members`
- `[workspace].exclude`

Only listed workspace members are discovered.

### JavaScript

Flux reads:

- root `package.json` `workspaces`
- root `pnpm-workspace.yaml`

Only listed workspace packages are discovered.

### uv / Python

Flux reads the root `pyproject.toml` and uses:

- `[tool.uv.workspace].members`
- `[tool.uv.workspace].exclude`

Only listed workspace members are discovered.

### Ignored directories during expansion

When expanding workspace globs, Flux ignores common generated and vendored directories:

- `node_modules`
- `.git`
- `target`
- `.venv`
- `dist`

This prevents false package discovery from installed dependencies or build artifacts.

## How Native Dependencies Work

Flux tracks native internal dependencies so it can understand topology and walk toward bridge declarations.

It only treats explicitly local/workspace references as internal workspace dependencies.

### Cargo internal dependency detection

Cargo dependencies are considered internal only if they use:

- `path = "..."`
- `workspace = true`

Flux ignores external crates, even if their names match workspace packages.

### JavaScript internal dependency detection

JavaScript dependencies are considered internal only if their version specifier uses:

- `workspace:`
- `file:`
- `link:`
- `portal:`

Flux ignores registry dependencies like `react`, `typescript`, or published internal package names.

### uv internal dependency detection

uv dependencies are considered internal only if the dependency is backed by a local/workspace source in:

- `[tool.uv.sources]`

Flux ignores normal published Python dependencies.

## Dev Dependency Policy

Dev-only dependency edges are ignored when building topology.

Ignored dependency types:

- Cargo `dev-dependencies`
- npm `devDependencies`
- uv dev groups / dev dependencies

Flux currently still considers certain non-dev dependency types where they are part of normal package relationships, such as Cargo `build-dependencies`, npm `peerDependencies`, and uv optional dependencies, but only when they are explicitly local/workspace references.

Flux also prints warnings to stderr when a workspace package references another workspace package from a special dependency section such as Cargo `dev-dependencies`, Cargo `build-dependencies`, npm `devDependencies`, or uv dev dependency groups. These warnings do not change planning; they are just a signal that the repo is depending on workspace packages through non-runtime sections.

## Task Definitions

Logical tasks are defined in a repo-level `flux.toml`.

Example:

```toml
[tasks.build]
depends_on = ["test"]
workspace_batchable = true
variables = ["mode"]
cargo = ["cargo", "build"]
npm = ["npm", "run", "build"]
pnpm = ["pnpm", "build"]
yarn = ["yarn", "build"]
bun = ["bun", "run", "build"]
uv = ["uv", "run", "python", "-m", "build"]

[tasks.test]
autoapply = "inherit"
cargo = ["cargo", "test"]
npm = ["npm", "run", "test"]
uv = ["uv", "run", "pytest"]

[tasks.publish]
cascade = "all"
cargo = ["cargo", "publish"]

[tasks.docker]
default = ["docker", "build", "."]
```

### Supported task variants

Each task can define commands for:

- `default`
- `cargo`
- `npm`
- `pnpm`
- `yarn`
- `bun`
- `uv`

`default` is a shared fallback command for tasks that are the same across ecosystems.

Example:

```toml
[tasks.docker]
default = ["docker", "build", "."]
```

For JavaScript packages, Flux prefers the detected package-manager-specific variant, then falls back to `npm`, then to `default` when present.

### `depends_on`

Tasks can depend on other logical tasks:

```toml
[tasks.build]
depends_on = ["test"]
cargo = ["cargo", "build"]

[tasks.test]
cargo = ["cargo", "test"]
```

When a task depends on another task, Flux schedules the dependency task on the same package before the dependent task.

This composes with cascades and bridges. If `build` appears on a package because it was an entrypoint, a cascaded dependency, or a bridged package, `build`'s task dependencies are applied there too, subject to the dependency task's own `autoapply` and `cascade` settings.

### `workspace_batchable`

Tasks can opt into workspace-native batching:

```toml
[tasks.check]
workspace_batchable = true
cargo = ["cargo", "check"]
```

This does not change planning semantics. Flux still plans package-task nodes first and respects dependency ordering.

Batching only happens at execution time, and only across tasks that are already unblocked in the same ready group.

Today this is implemented for Cargo commands. When multiple ready Cargo packages have the same logical task and that task is marked `workspace_batchable`, Flux can collapse them into a single workspace Cargo invocation such as `cargo check -p a -p b -p c`.

This is an execution optimization only. The dependency graph, task availability rules, and plan output still operate on package-task nodes.

### `ecosystem_depends_on`

Tasks can also declare ecosystem-scoped dependency searches.

Example:

```toml
[tasks.build]
npm = ["npm", "run", "build"]

[tasks.build.ecosystem_depends_on]
js = ["cargo:gen"]
```

This means:

- when Flux runs `build` on a JS package
- it searches the reachable dependency tree from that package
- any reachable Cargo package is asked to run `gen` before the JS `build` continues

This is different from `depends_on`:

- `depends_on` adds same-package task dependencies like `pkg:build -> pkg:test`
- `ecosystem_depends_on` searches the reachable package graph for another ecosystem and schedules a different task there

### `autoapply`

`autoapply` controls whether a task is available on a package without that package explicitly opting into it.

Default:

- `"none"`

Supported values:

- `"none"`
- `"inherit"`
- `"all"`

Example:

```toml
[tasks.test]
autoapply = "inherit"
cargo = ["cargo", "test"]
```

Behavior:

- `none`: only explicit opt-ins can receive the task
- `inherit`: the task can be inherited through bridges, task dependencies, or native cascade rules
- `all`: same as `inherit`, and if no package explicitly opts into the task, Flux seeds the task on every compatible package

### `cascade`

`cascade` controls how far a task can spread across packages.

Supported values:

- `"none"`
- `"bridge-only"`
- `"all"`

Default:

- `"bridge-only"`

Behavior:

- `none`: the task only runs on packages that explicitly opt into it
- `bridge-only`: the task can cross bridges, but it does not run on same-ecosystem dependency chains
- `all`: the task also runs on same-ecosystem dependencies and their dependencies

Examples:

```toml
[tasks.publish]
cascade = "all"
cargo = ["cargo", "publish"]
```

```toml
[tasks.publish-docker]
cascade = "none"
default = ["docker", "push", "${image}"]
```

### `variables`

Tasks can declare required variables:

```toml
[tasks.build]
variables = ["mode"]
npm = ["npm", "run", "build:${mode}"]
```

If a package opts into `build`, it must provide every variable named in `variables`.

This lets Flux fail early with a clear configuration error even if the command shape itself would not have surfaced the missing value until later.

The contract is:

- task definition in `flux.toml` declares which variables are required
- package opt-in in the native manifest provides the values
- task command may interpolate those values with `${name}`

Example:

```toml
[tasks.build]
variables = ["mode"]
npm = ["npm", "run", "build:${mode}"]
```

```json
{
  "name": "@repo/web",
  "flux": {
    "tasks": {
      "build": {
        "variables": {
          "mode": "production"
        }
      }
    }
  }
}
```

If `@repo/web` opted into `build` but did not supply `mode`, Flux would fail during task resolution with a configuration error instead of silently planning an invalid command.

`cascade = "all"` is useful for tasks that should deliberately run across same-ecosystem dependency chains, such as repo-specific release or publish flows.

## JavaScript Package Manager Detection

For JavaScript packages, Flux determines the package manager in this order:

1. `package.json` `packageManager`
2. lockfiles in the package directory or its ancestors
3. fallback to `npm`

Recognized lockfiles:

- `pnpm-lock.yaml`
- `yarn.lock`
- `bun.lock`
- `bun.lockb`
- `package-lock.json`

This affects task resolution and display labels.

Examples:

- JS package with pnpm uses the `pnpm` variant if present
- JS package with Yarn uses the `yarn` variant if present
- output labels show `[pnpm]`, `[yarn]`, `[bun]`, or `[npm]`

## Package Opt-In

Packages decide which logical tasks they participate in through native manifest metadata.

Simple opt-in still works:

- `tasks = ["build", "test"]`

But tasks can also be declared in a richer form with per-task variables.

### Cargo opt-in

```toml
[package.metadata.flux]
tasks = ["build", "test"]
```

With task variables:

```toml
[package.metadata.flux.tasks.build.variables]
profile = "release"
target = "wasm32-unknown-unknown"
```

### `package.json` opt-in

```json
{
  "name": "@repo/web",
  "flux": {
    "tasks": ["build", "lint"]
  }
}
```

With task variables:

```json
{
  "name": "@repo/web",
  "flux": {
    "tasks": {
      "build": {
        "variables": {
          "mode": "production"
        }
      }
    }
  }
}
```

### `pyproject.toml` opt-in

```toml
[tool.flux]
tasks = ["test"]
```

With task variables:

```toml
[tool.flux.tasks.build.variables]
mode = "release"
```

### Task variables

Package-level task opt-in can provide task-specific variables. Flux interpolates those variables into the task command from `flux.toml`.

Use `${name}` in task command parts:

```toml
[tasks.build]
npm = ["npm", "run", "build:${mode}"]
```

If a package opts into `build` with:

```json
{
  "flux": {
    "tasks": {
      "build": {
        "variables": {
          "mode": "production"
        }
      }
    }
  }
}
```

then Flux resolves:

```text
npm run build:production
```

There are two ways missing variables are caught:

- if the task declares `variables = ["mode"]`, Flux errors as soon as a package opts into that task without providing `mode`
- if a command references `${mode}`, Flux also errors during interpolation if `mode` is missing

Simple task-list opt-in is still fine when no task variables are needed. Use the richer task-map form only for packages that need per-task overrides.

### JavaScript task participation

Flux does not treat `package.json` scripts as explicit task opt-ins.

JavaScript packages participate in Flux tasks only when:

- they opt in through `flux.tasks`
- the task becomes available through `autoapply`
- the package is reached through bridge or cascade rules that allow that task

The `scripts` section still matters for your package manager's own behavior, but it does not make a package a Flux entrypoint by itself.

## Bridge Definitions

Bridges are declared at the package level in native manifests.

They represent cross-ecosystem dependencies that Flux must orchestrate manually.

### Cargo bridges

```toml
[package.metadata.flux]
bridges = ["js:@repo/ui", "uv:codegen"]
```

### `package.json` bridges

```json
{
  "name": "@repo/web",
  "flux": {
    "bridges": ["cargo:myko-rs"]
  }
}
```

### `pyproject.toml` bridges

```toml
[tool.flux]
bridges = ["cargo:core-types"]
```

## Scoped Bridge Syntax

Bridge targets can be written as:

- `cargo:<name>`
- `js:<name>`
- `uv:<name>`

Examples:

- `cargo:myko-rs`
- `js:@rship/ui`
- `uv:shared-lib`

Unscoped bridge names still work:

```toml
bridges = ["codegen"]
```

But unscoped names are only safe when that package name is unique across the whole repo. If the same name exists in multiple ecosystems, Flux raises an ambiguity error.

Scoped bridges are recommended.

## Planning Semantics

This is the most important rule set in Flux.

### 1. Entry points are opt-in

Only packages that opt into a task are chosen as task entrypoints.

Flux does not run a task on every package in the workspace.

### 2. Native dependencies do not imply same-task execution

If Cargo, npm, or uv already know how to build their own native dependency graph, Flux does not manually schedule the same task on every native dependency.

Example:

- Rust crate `a` depends on crate `b`
- crate `b` depends on crate `c`
- `a` opts into `build`

Flux schedules `a:build`, not `b:build` and `c:build`.

Cargo itself is expected to handle those native build relationships.

### 2a. Tasks can choose how far they cascade

If a task sets `cascade = "all"` in `flux.toml`, Flux may schedule that same task on native internal dependencies, but only when the task is also allowed to inherit there by `autoapply`.

Example:

- crate `a` depends on `b`
- `b` depends on `c`
- task `publish` sets `autoapply = "inherit"`
- task `publish` has `cascade = "all"`
- `a` opts into `publish`

Flux schedules:

- `c:publish`
- `b:publish`
- `a:publish`

### 3. Native dependencies are still traversed to find bridges

Flux walks native dependency chains while looking for bridge declarations.

Example:

- `a -> b -> c` are Cargo dependencies
- crate `c` has `bridges = ["js:d"]`
- `a` opts into `build`

Flux finds the bridge through `c` and schedules `d:build` before `a:build`.

### 4. Bridges carry the same task name

Bridges are not task-specific.

If Flux is planning `build`, a bridge means "run `build` on that bridged package too."

If Flux is planning `test`, the same bridge means "run `test` on that bridged package too."

### 5. Task dependencies run on the same package first

If a task declares `depends_on = ["test"]`, then every `pkg:build` node depends on `pkg:test`.

That applies anywhere the task appears:

- direct task entrypoints
- native cascaded packages
- bridged packages

If the dependency task sets `cascade = "none"`, Flux does not inherit that task onto other packages at all. It only runs on packages that explicitly opt into that task themselves.

### 6. Bridge targets follow `autoapply` and `cascade`

Bridges do not bypass task availability rules.

A bridged package gets task `X` when either:

- the package explicitly opts into `X`
- `X` sets `autoapply = "inherit"` or `autoapply = "all"`, and `cascade` is not `"none"`

This keeps bridge behavior predictable:

- `autoapply = "none"` means only explicit opt-ins can receive the task
- `cascade = "none"` means the task never spreads to other packages, even across bridges
- `cascade = "bridge-only"` means the task may cross bridges, but not same-ecosystem dependency chains
- `cascade = "all"` means the task may cross bridges and also run on same-ecosystem dependencies when `autoapply` allows inheritance

## Examples

### Example 1: simple mixed repo

`flux.toml`:

```toml
[tasks.build]
cargo = ["cargo", "build"]
pnpm = ["pnpm", "build"]
uv = ["uv", "run", "python", "-m", "build"]
```

Rust crate:

```toml
[package]
name = "myko-rs"
version = "0.1.0"
```

JavaScript package:

```json
{
  "name": "@rship/ui",
  "scripts": {
    "build": "vite build"
  },
  "flux": {
    "bridges": ["cargo:myko-rs"]
  }
}
```

If `@rship/ui` is an entrypoint for `build`, Flux will plan:

```text
@rship/ui:build [pnpm]
`-- myko-rs:build [cargo]
```

### Example 2: bridge discovered through native deps

Imagine:

- Cargo crate `a` depends on `b`
- `b` depends on `c`
- `c` bridges to JS package `d`
- `a` opts into `build`

Flux will plan:

```text
a:build [cargo]
`-- d:build [npm]
```

It will not separately schedule `b:build` or `c:build`.

## Output

### `graph`

`graph` shows package relationships at the workspace topology level.

Example shape:

```text
service [cargo] service/Cargo.toml
`-- shared [cargo] shared/Cargo.toml
```

### `plan build`

`plan build` shows the task plan.

Example shape:

```text
app:build [cargo]
`-- @repo/ui:build [pnpm] {mode=production}
```

For JS packages, the label uses the detected package manager instead of the generic `js` ecosystem.

## Plugin Architecture

Workspace support is implemented as ecosystem plugins.

Current built-in plugins:

- Cargo plugin
- JS plugin
- uv plugin

The shared core handles:

- package normalization
- common package metadata
- internal dependency linking
- graph building
- task planning

Each plugin owns:

- workspace discovery
- native manifest parsing
- task opt-in extraction
- bridge extraction
- native dependency extraction
- ecosystem-specific metadata like JS package-manager detection
- ecosystem-specific execution shaping such as workspace batching

## Current Limitations

Current known boundaries:

- `run` executes one execution unit at a time; parallel execution is not implemented yet
- workspace batching is currently implemented only for Cargo task commands
- bridge targets resolve by package identity, not by filesystem path
- bridge syntax supports ecosystem scoping, but not more advanced selectors
- inherited task availability is controlled by `autoapply` and `cascade`, so repo config needs to describe which tasks may spread
- uv does not have a native task system, so Python task participation is driven by Flux metadata and `flux.toml`

## Suggested Repo Setup

For a mixed repo, a good starting pattern is:

1. Define logical tasks once in `flux.toml`
2. Opt in only true task entrypoints in native manifests
3. Add bridges only where work must cross ecosystems
4. Prefer scoped bridges like `cargo:myko-rs`

That keeps Flux focused on orchestration rather than replacing each ecosystem's native build behavior.
