---
kind: capability
surfaces: [daemon]
issues: [265]
---
User can now: run `dormantd` on Windows and have `dormantctl` talk to it over a named pipe.

Detail: The daemon's IPC server now serves the Windows named pipe `\\.\pipe\dormant-<username>` alongside the Unix-domain socket, reusing the same transport-generic connection handler. The first pipe instance is created with `first_pipe_instance(true)` so a second daemon is refused with the same "already in use by a running daemon" error the Unix path produces.