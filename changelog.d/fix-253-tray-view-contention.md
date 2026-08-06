---
kind: fix
surfaces: []
issues: [253]
---
The Linux tray menu now refreshes after daemon state changes instead of staying
frozen until a click, and no longer fabricates an unreachable view when the
shared tray state is briefly contended.
