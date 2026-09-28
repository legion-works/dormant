---
kind: capability
surfaces: [daemon, config, docs]
issues: []
readme_bullet: "**Shared-display ownership hooks** — fire a command or MQTT publish on observed gain/loss so USB and notifications follow the panel owner without writing DDC."
---
User can now: publish an MQTT action when a shared monitor's input switches to this machine outside dormant, so a USB KVM can follow the observed panel owner, with the gain hook gated on a confirmed-present zone by default so a panel hunting inputs overnight cannot wake downstream consumers.

Detail: when `[displays.*.hooks].observed_gain_requires_presence` is true (the default), the post-hoc `on_observed_gain` hook defers until at least one driving rule's zone resolves present; a Some(false) verdict defers the hook and re-evaluates every subsequent poll tick, dropping the deferral on a committed ownership loss. Knob false preserves the original fire-on-every-commit behaviour.