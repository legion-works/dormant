---
kind: fix
surfaces: []
issues: []
---
Detail: The ESPHome examples promised the LD2410 gate thresholds were adjustable from Home Assistant, but every one of them read `unknown` — the values live in the radar module and nothing ever asked for them. Tiers 2 and 3 now declare the `query_params` button and press it on boot.
