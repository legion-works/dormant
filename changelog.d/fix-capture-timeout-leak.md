---
kind: fix
surfaces: [daemon]
issues: []
---
Detail: The daemon now keeps its PipeWire capture connection alive when a blanked screen produces no frame, preventing KWin memory from growing with a new buffer pool every sampling timeout.
