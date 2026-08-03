---
kind: fix
surfaces: []
---
Active wear sampling now renegotiates its portal session after persistent
capture failures, including display hotplug recovery. `disable-sampling`
without `--forget` can now be reversed with `enable-sampling` without another
consent dialog.
Cooldown status retains its distinct `wear_sampling_cooldown` diagnostic
reason while the sampler renegotiates the saved portal session.
