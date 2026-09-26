---
kind: fix
surfaces: []
issues: []
---
Detail: Home Assistant MQTT discovery marks room zones and presence sensors as occupancy binary sensors. A colliding identifier pair emits one warning per discovery flush; reconnect flushes may report it again. Startup and run-loop errors include their full anyhow context chain in the daemon log.
