---
kind: fix
surfaces: []
issues: [401]
---
Detail: A config reload stops incoming controls and drains accepted controls before it saves display state. Manual display commands that complete during reload keep their state in the new engine. Pending manual commands get a bounded wait. If a wake does not complete in time, the new engine keeps its retry driver.
