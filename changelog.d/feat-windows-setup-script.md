---
kind: capability
surfaces: [docs]
issues: [265]
---
User can now: run `scripts\windows-setup.ps1` on Windows to build, install, bootstrap a config, and smoke-test dormant in one pass, with a consent-gated blank/wake test that arms an independent wake process so a display cannot be left dark.

Detail: there are no published Windows release artifacts, so the script builds from source. It checks the toolchain before building, resolves the install path from `cargo metadata` rather than assuming `target/release`, never overwrites an existing config, and prints a pass/fail summary naming what the run did not prove.
