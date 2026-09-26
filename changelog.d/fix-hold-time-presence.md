---
kind: fix
surfaces: [daemon, docs]
issues: []
---
Detail: `hold_time` now defers an `off` event for presence sensors as well as motion sensors. Configurations whose `hold_time` exceeds the effective `stale_timeout` now fail validation with `sensor '<id>' hold_time <duration> exceeds effective stale_timeout <duration>; lower hold_time or raise stale_timeout`. Raise `stale_timeout` to at least `hold_time` or lower `hold_time`; when omitted, the effective timeout comes from `[daemon].stale_sensor_timeout` (default `5m`).
