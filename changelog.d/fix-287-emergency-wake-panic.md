---
kind: fix
surfaces: []
issues: [287]
---
Detail: `dormantctl emergency-wake` no longer reports success for only the displays whose wake tasks completed when another task panics. The panicked display is now listed as failed with the panic detail.
