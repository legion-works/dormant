---
kind: fix
surfaces: []
---
Detail: `dormantctl doctor mqtt` no longer reports a healthy sensor when the broker only holds a stale retained value. A retained message is delivered the moment the probe subscribes, so a topic that stopped publishing looked identical to a live one — the probe passed while the daemon marked the same sensor unavailable and the display would not blank. It now reports whether the value was observed live, and names the staleness risk when only a retained one arrives.
