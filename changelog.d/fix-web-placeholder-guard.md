---
kind: fix
surfaces: []
issues: []
---
Detail: A release build made without first building the web UI produced a daemon that served a blank dashboard instead of the real one. Such a build now fails with the command to fix it, and `scripts/deploy-local.sh` builds and verifies the whole set in one step.
