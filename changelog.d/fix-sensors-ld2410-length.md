---
kind: fix
surfaces: []
issues: [277]
---
Detail: A corrupt oversized LD2410 frame length could stall parsing and freeze the last presence state indefinitely. The parser now discards impossible lengths and continues to the next valid frame.
