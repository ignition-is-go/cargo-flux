# Contributing

Use these guidelines when preparing or reviewing a change. Explain material
exceptions in the pull request, including outstanding validation and who owns
the follow-up.

## Prepare the change

- Keep the diff focused on the requested outcome.
- Describe intended behavior changes and any migration needed by existing callers.
- Add focused regression coverage for changed behavior. Exercise the real CLI,
  plugin or consumer boundary where practical.
- Update documentation and examples for affected commands and configuration.

## Validate before merging

Run the local Rust checks appropriate to the change.

```sh
cargo fmt --all --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

Normally wait for Linux `check`, `windows-check` and workflow validation to pass
on the commit being merged. The [CI workflow](.github/workflows/ci.yml) defines
the platform and published-binary compatibility checks. For platform-specific
changes, record coverage for each affected platform.

For affected behavior, verify existing commands, configuration, cache keys and
archive-reader compatibility. Check that reusable outputs stay warm and that
failure, retry and restore paths preserve consistent state. Keep unknown
provenance explicit. Verify observers against the real consumer, including
repeated or concurrent observations where those operations can overlap.

## Review the final diff

Resolve blocking correctness findings and explain remaining trade-offs. Tie
validation claims to the exact commit and behavior exercised. After additional
commits, review the new diff and repeat the affected checks. State any behavior
or platform that remains unverified.

## Account for release automation

Use a commit title that reflects the change. Release versioning uses `fix:`,
`feat:` and breaking-change markers. Documentation-only changes normally use
`docs:`.

The CI `release` job runs after the Linux and Windows checks pass on `main`.
It invokes the [release task](flux.toml) to calculate and stamp versions, then
push a release commit and tag. Account for that direct-push path when changing
repository workflow or permissions.

Report merge, publication and downstream runtime validation separately, with
evidence for each claimed state.
