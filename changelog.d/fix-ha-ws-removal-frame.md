---
kind: fix
surfaces: []
---
Detail: when Home Assistant deleted a subscribed entity, the display could stop blanking while the stale sensor state remained for up to five minutes before the sensor became unavailable; the removal frame now marks that sensor unavailable immediately and identifies the deletion correctly.
