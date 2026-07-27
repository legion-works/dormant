---
kind: fix
surfaces: []
---

The `user-activity` inhibitor now binds `ext_idle_notifier_v1` at version 2
and uses the input-idle notification when the compositor offers it, so
application idle inhibitors (a browser tab holding a WebRTC or video
inhibitor) can no longer hold a blank on a vacant room. Observed live: an
idle browser PeerConnection kept an OLED panel awake through 82 minutes of
absence. v1-only compositors keep the legacy inhibitor-respecting behavior.
