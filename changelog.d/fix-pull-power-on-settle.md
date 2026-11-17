---
kind: fix
surfaces: []
issues: [408]
---
Prevent duplicate input writes while a DDC/CI panel starts after a power-on command. Pull and push retry input reads for a maximum of eight seconds after power-on.
