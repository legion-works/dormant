## Description

<!-- What does this PR do? Why? -->

## Checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo build --workspace` passes
- [ ] `cargo doc --workspace --no-deps` passes
- [ ] `cargo deny check` passes
- [ ] New public items have doc comments
- [ ] Tests cover the changes
- [ ] No hard-coded magic numbers — constants from `defaults.rs` or config keys
- [ ] Fail-safe presence policy preserved (unavailable = present)
- [ ] No telemetry, no phone-home
- [ ] Conventional commit format used
