## Description

<!-- What does this PR do? Why? -->

## Checklist

- [ ] Relevant `scripts/gates/*.sh` entry points pass (`fmt.sh`, `clippy.sh`, `rust-tests.sh`, and `rustdoc.sh` for Rust changes)
- [ ] `rust-tests.sh` passed both nextest and doctests
- [ ] Web UI, mdBook, dependency-policy, and MSRV gate scripts ran when their paths changed
- [ ] No test was rerun to clear a flake; any flake follows `.github/flake-ledger.toml`
- [ ] New public items have doc comments
- [ ] Tests cover the changes
- [ ] Flake-fix PRs include Ubuntu and macOS reload-stress evidence (100/100 per target)
- [ ] No hard-coded magic numbers — constants from `defaults.rs` or config keys
- [ ] Fail-safe presence policy preserved (unavailable = present)
- [ ] No telemetry, no phone-home
- [ ] Conventional commit format used
