---
kind: fix
surfaces: []
---

Panel-exposure heat map normalizes from zero instead of min-max, so a uniformly-worn panel renders at full intensity (every cell at 1.0) instead of collapsing to flat grey / zero heat — indistinguishable from an unsampled panel. The detail response now also exposes `max_cell_hours` so the legend can label real hours instead of inferring them from the normalized heat.
