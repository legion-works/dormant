---
kind: fix
surfaces: []
issues: []
---
Detail: The libmpv render tests bounded frame production with real-clock deadlines tight enough to fail on a loaded machine rather than on a regression — one allowed 200ms for mpv to initialise and decode its first frame. All five polling sites now share a single documented hang-detector timeout.
