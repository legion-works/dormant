---
kind: fix
surfaces: [core, tray]
issues: []
---
Detail: `StateSnapshot.displays[*].rules` now lists the ids of the rules that drive each display, sourced from the same `cfg.rules` enumeration the engine already consults for presence and input-wake-hold. A display with no driving rule is marked on the tray tooltip line with `no automatic rule — dormant won't blank it on its own` so the operator can distinguish a manual-only panel from a covered one without leaving the tray. The field is `#[serde(default)]` so older daemon JSON still parses, and an empty list still serializes as `"rules": []`.
