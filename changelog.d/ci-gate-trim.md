---
kind: improvement
surfaces: []
issues: []
---
Detail: CI and local checks keep only the gates that block real problems. These changes apply:
- Required checks are now `clippy`, `test`, `webui`, `render`, `deny`, `docs`, `gitleaks`, `mqtt-integration`, `windows-portability`, `macos-test` and `policy`. The other jobs still run and report.
- A required job can be re-run on the same commit.
- The pull request title check is removed.
- The macOS nightly soak jobs are removed.
- These checks report warnings and no longer block: the shell lint, raw-sleep, changelog-fragment and gate-parity policy steps, the pre-commit style and lint hooks, and the 30-day flake-ledger diagnosis deadline.
- The nextest-policy and soak-parity checks are removed.
- `cargo deny` no longer fails on unmaintained-only advisories.
- `scripts/deploy-local.sh` keeps the live-config check and the running-binary check, and drops its copies of the placeholder check that `dormant-web`'s build script makes.
