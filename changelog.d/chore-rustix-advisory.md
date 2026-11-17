---
kind: fix
surfaces: []
issues: []
---
Detail: Updated `rustix` to 1.1.6, clearing RUSTSEC-2026-0486: its extended-attribute functions could mark unwritten bytes as initialized when given a zero-length buffer. A documentation table in the MQTT broker URL parser now has a column for each cell, so the current `rustdoc` build accepts it.
