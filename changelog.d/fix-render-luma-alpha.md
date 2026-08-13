---
kind: fix
surfaces: []
issues: [283]
---
Detail: Translucent screensaver pixels were assigned the wrong luma because alpha was applied before sRGB linearization. Luma scans now composite over black in linear light.
