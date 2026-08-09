---
kind: fix
surfaces: []
---
`dormantctl doctor mqtt` no longer reports a sensor as healthy when the daemon marks it unavailable and the display cannot blank because the topic has only a stale retained value; it now says that the broker holds a value but nothing was published during the probe window.

Detail: the probe distinguishes retained values delivered on subscribe from live MQTT publishes and warns when an on-change topic without a heartbeat may become stale after `stale_timeout`.
