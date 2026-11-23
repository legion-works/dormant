---
kind: fix
surfaces: [displays]
issues: [367]
---
Detail: The daemon saves Samsung IP Control tokens on Windows and uses them after a restart. The tokens are saved under `%LOCALAPPDATA%`; when neither `LOCALAPPDATA` nor `USERPROFILE` is set, they stay in memory only.
