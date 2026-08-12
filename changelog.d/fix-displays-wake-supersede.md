---
kind: fix
surfaces: []
issues: [280]
---
Detail: A display blank arriving while wake recovery was re-probing could be undone by a late wake attempt. Recovery now checks for supersession before every re-probe wake, leaving the newer blank in force.
