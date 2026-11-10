---
kind: fix
surfaces: [core]
issues: []
---
Detail: After a render failure, the stage keeps its configured dwell before the next stage starts. A missing render surface does not cause hardware power-off immediately. Presence can cancel the dwell without a hardware blank. Terminal render failures keep their previous behavior.
