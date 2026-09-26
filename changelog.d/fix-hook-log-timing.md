---
kind: fix
surfaces: [daemon]
issues: []
---
Hook logs now record `hook_started` when execution begins and include elapsed milliseconds on success, failure, and timeout.
