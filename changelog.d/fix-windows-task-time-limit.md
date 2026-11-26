---
kind: fix
surfaces: [docs]
issues: [375]
---
Detail: `scripts\windows-setup.ps1 -RegisterLogonTask` now registers the logon task with no run-time limit. Task Scheduler stops a task 72 hours after it starts unless its execution time limit is zero, so the daemon stopped after three days of uptime. The Windows installation guide now tells you to clear "Stop the task if it runs longer than" for a task that you register by hand.
