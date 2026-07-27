---
kind: capability
surfaces: [readme, chapter]
readme_bullet: "**Web UI v3** — full config-schema parity in the web editor (coordination, keymaps, input filters, hooks), a new shared-display switching view with live pull/push feedback, event history across reloads, doctor checks grouped by subject, panel-wear detail split by seeded/measured time, and a security-posture card with honest boundary notes."
---
User can now: edit every config section — coordination, keymap, input filter, and
hooks — from the web Settings editor, inspect shared-display ownership with live
pull/push feedback on the new Switching view, and see event history preserved
across page reloads.

Detail: six-tab Settings layout (Daemon, Sensors, Zones, Rules, Displays,
Coordination). Switching view surfaces `Ownership` daemon events over the
WebSocket and shows both machines' input codes, the panel state, poll cadence,
and agreement verdict. `/api/events/recent` seeds the event log on page load so
history survives a reload. Doctor checks are grouped by subject. Panel-wear
detail now splits wear time into seeded (pre-existing) and measured (new
samples) with a per-cell heat map. The rollback banner surfaces the recovery
command when one is configured. A security-posture card honestly states what the
Host and Origin guards defend, and what they don't. Hook editing is gated behind
the new `daemon.hook_edit_enabled` flag (default off); without it hooks are
read-only everywhere.
