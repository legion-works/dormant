---
kind: fix
surfaces: [docs]
issues: []
---
Detail: the ESPHome example tiers now expose the LD2410C's onboard Bluetooth radio as a switch defaulting to off. The module ships with BLE advertising enabled; Home Assistant's `ld2410_ble` integration auto-discovers and connects to it, recording motion with no consumer. The state persists in the module's NVM.
