---
kind: fix
surfaces: [cli, scripts]
issues: []
---
Detail: `dormantctl pair samsung` no longer claims the TV is showing its
"Allow dormant" prompt before any connection exists; it says to accept the
prompt if the TV shows it and names the 60 s timeout. `scripts/deploy-local.sh`
installs each binary through a temp file and rename, so a binary held open by
another process no longer blocks the install; any failed install exits
non-zero, and after a restart the script checks that the running daemon is the
installed binary.
