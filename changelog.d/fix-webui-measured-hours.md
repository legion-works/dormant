---
kind: fix
surfaces: [webui]
issues: []
---
Detail: The display page in the web UI no longer shows 0 measured on-hours for a panel that reports a lifetime counter (VCP `0xC0`). The page subtracted the panel's counter from the measured total, but the measured total never includes it. The page now shows the measured total and the panel's counter as two separate figures.
