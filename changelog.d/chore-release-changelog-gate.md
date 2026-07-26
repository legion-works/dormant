---
kind: improvement
surfaces: []
---

Wire `release-prep.py --check` into the release pipeline as a reusable workflow
(`release-changelog-gate.yml`) registered in dist's `global-artifacts-jobs`.
A changelog surface violation now blocks the GitHub release from being
published. The gate is generated via `dist generate`, not hand-edited into
`release.yml`.
