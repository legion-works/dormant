---
kind: capability
surfaces: [daemon]
issues: []
---
On macOS, a shared-display hook can skip its wake command when every online display is already awake. Failed or empty sleep-state probes still run the command.
