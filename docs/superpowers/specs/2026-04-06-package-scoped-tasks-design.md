# Package-Scoped Task Running

Add `package:task` syntax to `run` and `plan` commands so users can target a single package as the entrypoint while still resolving the full dependency graph.

## CLI Syntax

```bash
cargo flux run build          # existing: all opted-in packages
cargo flux run my-crate:build # new: only my-crate as entrypoint
cargo flux plan my-crate:build
cargo flux plan my-crate:build --ordered
```

## Parsing

The `task` argument is parsed for a `:` separator. If present, the left side is the package name and the right side is the task name. If no `:` is present, the entire string is the task name (existing behavior).

Parsing happens in `main.rs` after CLI parsing, not in clap itself. The `task` field in `cli.rs` stays a plain `String`.

## Graph Planning

The graph planning methods (`task_plan`, `task_ready_groups`, `render_task_plan_tree`) gain an optional `package_filter: Option<&str>` parameter.

When `package_filter` is `None`, behavior is unchanged — all packages that participate in the task are used as entrypoints.

When `package_filter` is `Some(name)`, only the named package is used as the entrypoint. The rest of the planning works unchanged:

- Bridge dependencies are still traversed
- Task dependencies (`depends_on`) still apply
- Cascade rules still apply
- `autoapply` rules still apply

The filter only controls which packages are chosen as entrypoints. It does not limit the resolved dependency graph.

## Error Cases

- Package name not found in workspace: error listing available packages.
- Package doesn't participate in the task (not opted in, not autoapplied): error.

## Changes

| File | Change |
|------|--------|
| `src/main.rs` | Parse `package:task` syntax, pass filter to graph methods |
| `src/graph.rs` | Add `package_filter` parameter to `task_plan`, `task_ready_groups`, `render_task_plan_tree` |
| `src/main.rs` tests | Add CLI parsing tests for `package:task` syntax |
| `src/graph.rs` tests | Add planning tests with package filter |

## Testing

- Parse `"build"` as `(None, "build")`.
- Parse `"my-crate:build"` as `(Some("my-crate"), "build")`.
- Parse `"@scope/web:build"` as `(Some("@scope/web"), "build")` — scoped npm packages contain `/` but not `:`.
- Planning with filter resolves only the filtered package's subgraph.
- Error when filtered package doesn't exist.
- Error when filtered package doesn't participate in the task.
