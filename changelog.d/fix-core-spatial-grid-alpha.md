---
kind: fix
surfaces: []
issues: [283]
---
Detail: Active wear sampling could misattribute translucent captured pixels because alpha was applied before sRGB linearization. The spatial luma reducer now composites over black in linear light, matching screensaver scans.
