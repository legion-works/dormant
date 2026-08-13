---
kind: fix
surfaces: []
issues: [282]
---
Detail: Motion sensors now reject hold_time values longer than their effective stale_timeout instead of silently dropping the pending absence hold.
