---
kind: fix
surfaces: []
issues: [254]
---
Pausing or resuming blanking now reaches the tray and web UI immediately.
Pause changes no display phase, so it previously emitted no daemon event and
left every event-stream consumer showing the stale unpaused state.
