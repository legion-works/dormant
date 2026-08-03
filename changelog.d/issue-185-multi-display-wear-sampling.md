---
kind: improvement
surfaces: []
issue: 185
---
Active wear sampling now runs on every display in `sampled_displays` at once: each selected display gets an independent sampler with its own ScreenCast consent record (`screencast-consent-<display>.json`), PipeWire stream, and lifecycle status. Upgrading from a singular `sampled_display` config carries the existing consent over with a one-way copy on first boot — no re-prompt. `dormantctl wear enable-sampling`/`disable-sampling` take `--display <id>` (required when more than one display is selected), the web bridge accepts `?display=<id>`, doctor reports one redacted result per display, and the web WearCard shows one independent row per display.

Detail: the daemon keeps a per-display sampler registry keyed by display id with independent cancellation tokens; the CLI resolves the target from the daemon's per-display status map and sends the additive `WearSampling*For { display }` IPC variants when a display is named.
