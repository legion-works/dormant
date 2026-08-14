---
kind: fix
surfaces: []
issues: []
---
Detail: `scripts/deploy-local.sh` now installs the binary cargo actually built. With `CARGO_TARGET_DIR` set, it verified and installed a stale artifact from a previous build, so a deploy could silently ship old code. The release-build SPA guard had the same class of bug and read another worktree's bundle.
