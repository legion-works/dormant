---
kind: fix
surfaces: []
issues: [285]
---
Detail: Home Assistant publishing no longer exits permanently when the daemon cannot answer the startup state snapshot. The publisher continues with an empty snapshot and fills retained state from later events or reconnects.
