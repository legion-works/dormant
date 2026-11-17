---
kind: improvement
surfaces: [ci]
issues: [399]
---
Detail: The reload smoke test waits for the defensive wake to complete before it checks that manual-only displays stay off. A slow wake command tests this wait.
