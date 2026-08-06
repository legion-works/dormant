---
kind: fix
surfaces: []
---
Fix a false alarm in the wear heat map: a panel with mean exposure below 1 h no longer renders a full-red hotspot for a +20% relative deviation. Below the 1-hour floor the map renders a neutral colour and displays an "insufficient wear data (<1h mean)" caption instead.
