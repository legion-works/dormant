---
kind: capability
surfaces: [daemon, config, docs]
issues: []
---
User can now: publish an MQTT action when a shared monitor's input switches to this machine outside dormant, so a USB KVM can follow the panel owner.

Detail: The `on_observed_gain` hook runs only while a rule driving the display sees someone present. A gain with nobody present waits, and the hook runs once presence is confirmed if this machine still owns the display; losing the display first cancels it. Offline sensors do not count as present. Displays with no rule are not gated. Set `[displays.<id>.hooks] observed_gain_requires_presence = false` to run the hook on every gain.
