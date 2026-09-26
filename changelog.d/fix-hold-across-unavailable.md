---
kind: fix
surfaces: [sensors]
---
Detail: a sensor's hold_time no longer resets when its source briefly goes unavailable, so an MQTT reconnect cannot clear a zone early
