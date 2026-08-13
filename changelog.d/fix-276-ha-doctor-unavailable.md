---
kind: fix
surfaces: []
issues: [276]
---
Detail: `dormantctl doctor` no longer reports a passing Home Assistant entity when HA says the sensor is unavailable. It now fails that entity with an explicit unavailable/unknown detail so the operator can distinguish a broken sensor from an unreachable probe.
