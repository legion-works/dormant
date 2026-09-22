---
kind: improvement
surfaces: [ci]
issues: [265]
---
Detail: The Windows portability job now builds, tests, and lints the workspace (with the `web-ui` feature) instead of only running `cargo check`, so foreign-target warnings and test-compile breakage fail the gate.