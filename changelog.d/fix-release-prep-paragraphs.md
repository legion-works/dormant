---
kind: fix
surfaces: []
---

`scripts/release-prep.py` is now paragraph-aware: `User can now:` and
`Detail:` markers absorb every following non-blank, non-marker line so a
multi-line paragraph is preserved verbatim instead of being silently
truncated to the first physical line, and a `fix` fragment that omits
`Detail:` now compiles a `Fixed` bullet from its plain body. Both bugs
hit the v0.8.0 release notes.