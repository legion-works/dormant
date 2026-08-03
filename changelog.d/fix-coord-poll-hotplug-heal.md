---
kind: fix
surfaces: []
---
Shared-display ownership polling now heals stale DDC/CI controller state after
display hotplug without restarting the daemon. Healing is thresholded and uses
bounded backoff so an unreachable panel does not trigger a re-probe storm.
