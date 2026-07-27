---
kind: fix
surfaces: []
---

The ESPHome radar example now ships a 5-minute retained state heartbeat
(previously a commented-out suggestion), so a stably-detected occupant no
longer starves dormant's silence-based staleness clock into marking a
healthy sensor unavailable. Docs recommend `stale_timeout = "6m"` to match.
