---
kind: fix
surfaces: []
issues: []
---
Detail: Reload receipt tests disable the config watcher when they send their own reload requests. This prevents the watcher from starting a second reload.
