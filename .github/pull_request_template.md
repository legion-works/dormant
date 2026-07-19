## Description

<!-- What does this PR do? Why? -->

## CI evidence

- First failing run: <!-- URL -->
- Stress evidence: <!-- links or run IDs that prove the fix -->

## Checklist

- [ ] Relevant `scripts/gates/*.sh` entry points pass (`fmt.sh`, `clippy.sh`, `rust-tests.sh`, and `rustdoc.sh` for Rust changes)
- [ ] `rust-tests.sh` passed both nextest and doctests
- [ ] Web UI, mdBook, dependency-policy, and MSRV gate scripts ran when their paths changed
- [ ] The first run was classified; any flake updates `.github/flake-ledger.toml`, fixes the root cause, and is followed by a new commit
- [ ] New public items have doc comments
- [ ] Tests cover the changes
- [ ] Flake-fix PRs include proving Ubuntu and macOS soak evidence (10 runs per target)
- [ ] No hard-coded magic numbers — constants from `defaults.rs` or config keys
- [ ] Fail-safe presence policy preserved (unavailable = present)
- [ ] No telemetry, no phone-home
- [ ] Conventional commit format used
