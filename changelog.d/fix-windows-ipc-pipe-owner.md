---
kind: fix
surfaces: [daemon, cli, tray]
issues: [417]
---

Detail: On Windows, `dormantctl` and the tray check that the current user owns the named pipe before they send data. The daemon uses the same owner with or without elevated privileges. Clients open the pipe without giving the server an impersonation-level token. Update and restart an elevated daemon before you use the new client.
