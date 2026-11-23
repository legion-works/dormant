---
kind: fix
surfaces: []
issues: [372]
---
Detail: Each Windows IPC pipe instance uses a protected access list that grants access only to the daemon user's SID. If the daemon cannot build that list, the daemon does not start.
