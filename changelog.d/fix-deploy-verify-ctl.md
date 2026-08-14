---
kind: fix
surfaces: []
issues: []
---
Detail: `scripts/deploy-local.sh` verified only the daemon while building and installing three binaries. It now also checks that the built `dormantctl` can validate the config this host runs, catching a build that silently lost the `render` feature.
