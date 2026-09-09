---
kind: fix
surfaces: [daemon, config]
issues: []
---
Detail: Shared displays now stop treating a rapidly flapping input-source readback as ownership changes. The daemon holds itself not-owned until the signal settles, preventing repeated blank/wake ladder churn.
