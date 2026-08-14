---
kind: fix
surfaces: []
issues: [274]
---
Detail: Screensaver crossfades no longer flash the new image at full opacity before the fade begins. Every frame committed during a transition is now blended against the outgoing image, including the first frame of the fade.
