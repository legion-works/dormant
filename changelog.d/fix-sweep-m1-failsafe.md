---
kind: fix
surfaces: []
---

Three fail-safe fixes: the daemon now starts with an unreachable display in a
degraded state (healing on first command) instead of crash-looping under
systemd/launchd; a sensor with a live retained `online` availability topic is
no longer marked unavailable by state-topic silence (silence means unchanged —
LWT `offline` still flips it immediately); and input-wake during a falsely
vacant zone now holds the display awake (`rules.<id>.input_wake_hold`, default
2m, `0s` disables) so a wrong sensor can no longer re-blank a typing user
every grace period.
