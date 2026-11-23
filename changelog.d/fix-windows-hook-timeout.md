---
kind: fix
surfaces: [daemon]
issues: [369]
---
Detail: On Windows, a hook timeout stops the hook process. Processes that the hook starts can continue to run.
