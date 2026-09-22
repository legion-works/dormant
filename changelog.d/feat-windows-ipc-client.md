---
kind: capability
surfaces: [cli, tray]
issues: [265]
---
User can now: run `dormantctl` on Windows and have it talk to `dormantd` over the named pipe.

Detail: The IPC client now connects to the Windows named pipe `\\.\pipe\dormant-<username>` alongside the Unix-domain socket, retrying briefly on `ERROR_PIPE_BUSY` and reporting the same "is the daemon running?" hint for a missing pipe. Read deadlines use a reader thread with a channel timeout, since a named-pipe handle has no `set_read_timeout`.