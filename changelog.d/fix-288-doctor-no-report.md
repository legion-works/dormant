---
kind: fix
surfaces: []
issues: [288]
---
Detail: `dormantctl doctor` no longer reopens USB sensors after a reachable daemon returns `ok` without a report. It now reports the malformed daemon response as failed and preserves the live daemon's ownership of the port.
