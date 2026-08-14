---
kind: fix
surfaces: []
issues: [273]
---
Detail: The black-overlay fallback now recreates its shared-memory buffer after a live output resize, so it no longer blanks only a stale top-left rectangle.
