---
kind: fix
surfaces: []
---
LD2410C ESPHome example defaults now match hardware-calibrated values that
prevent false presence from seating behind the desk.

Detail: `still_energy_floor` lowered from 30 to 28 based on per-scenario
still-energy distributions; `near_cutoff` reduced from 2.5 m to 1.6 m so the
distance fallback covers only the desk chair, not the couch behind it.
