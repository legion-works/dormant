---
kind: improvement
surfaces: [daemon, config, docs]
issues: []
---
Detail: The `on_observed_gain` hook runs only while a rule driving the display sees someone present, so a monitor hunting inputs overnight no longer moves a USB KVM. A gain with nobody present waits, and the hook runs once presence is confirmed if this machine still owns the display; losing the display first cancels it. Offline sensors do not count as present, and displays with no rule are not gated. Set `[displays.<id>.hooks] observed_gain_requires_presence = false` to run the hook on every gain.
