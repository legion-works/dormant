---
kind: fix
surfaces: [daemon]
issues: [369]
---
Detail: On Windows, a hook timeout stops the hook process instead of waiting for it to exit on its own. Processes started by the hook can continue to run.
