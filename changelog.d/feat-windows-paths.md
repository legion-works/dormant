---
kind: capability
surfaces: [daemon, cli]
issues: [265]
---
User can now: run dormant on Windows and have it resolve its config, state, named-pipe socket, and lock paths in native locations.

Detail: Windows path derivation added to dormant-core — config under `%APPDATA%`/`%PROGRAMDATA%` (with a `%XDG_CONFIG_HOME%` override), state and lock under `%LOCALAPPDATA%`, and the socket as the named pipe `\\.\pipe\dormant-<username>`.
