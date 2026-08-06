---
kind: fix
surfaces: []
---
Repeated shared-display switch requests now avoid re-running hooks or re-writing an input that is already active; `dormantctl switch --force` reasserts it when needed.
