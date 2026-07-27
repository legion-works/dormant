---
kind: improvement
surfaces: []
---

The web UI sidebar offers a dismissible "Star the repo" link — it tries the
local GitHub CLI first and falls back to opening the repo page, and one click
(star or dismiss) hides it permanently via a server-side flag so it never
nags twice.
