---
kind: fix
surfaces: []
issues: []
---
Detail: `scripts/deploy-local.sh` now installs the exact files cargo reports building, instead of reconstructing their paths. The previous mtime-based freshness check was unsound in both directions and rejected valid builds.
