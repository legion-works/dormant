---
kind: improvement
surfaces: [daemon]
issues: [265]
---
Detail: The daemon's IPC connection handler is now generic over the stream type, so a non-Unix transport can be layered on without touching request handling.