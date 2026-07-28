---
kind: fix
surfaces: []
---

A `power_off` blank on a shared macOS DDC/CI panel can be unrecoverable
(USB-C link and hub drop, VCP writes go to a dead device, recovery
requires physically power-cycling the monitor). Dormant now emits a
load-time semantic warning and a `dormantctl doctor` failure for every
display wired into that topology; setting `displays.<id>.power_off_opt_in
= true` acknowledges the risk and silences both. The opt-in adds no
recovery mechanism of its own — see `docs/src/displays.md` for the full
hazard description and the audio-safe alternatives.