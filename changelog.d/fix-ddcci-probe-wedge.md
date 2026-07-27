---
kind: fix
surfaces: []
---

Two DDC/CI fixes from live shared-panel switching: a controller whose
startup probe failed (panel held by the peer machine, display link down)
now re-probes on the first command instead of refusing every blank, wake,
and switch with "controller not probed" until the daemon is restarted; and
input-switch verification reads now follow an escalating retry schedule
(~3.5s) so the panel's post-switch re-sync garble no longer reports a
successful switch as a DDC checksum failure.
