---
kind: fix
surfaces: [displays]
issues: [265]
---
Detail: the `command` display controller hard-coded `sh -c`, which does not exist on a stock Windows install, so a Windows config using it validated fine and then failed every blank and wake with a spawn error. The controller now runs `cmd /C` on Windows.
