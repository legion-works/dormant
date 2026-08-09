# ESPHome LD2410C configurations

Pick one complete file; each tier is independently flashable. Tier 2 is the
recommended starting point. Tier 1 is for “prove the hardware works first”.
For the ESP-over-WiFi versus direct USB-serial topology discussion, see
[`docs/src/sensors.md`](../../docs/src/sensors.md).

| Tier | Use case | Transports | Gives you | Costs |
| --- | --- | --- | --- | --- |
| 1 · `01-minimal-mqtt.yaml` | Prove presence hardware | WiFi + MQTT | Native LD2410 occupancy, moving, and still entities | No native API, calibration, zones, or heartbeat |
| 2 · `02-standard-desk.yaml` | Desk that blanks reliably | ESPHome API + MQTT | Energy-gated desk presence, near/far zones, tuning controls, heartbeat | More entities and a short calibration exercise |
| 3 · `03-advanced-kvm.yaml` | One keyboard, two machines | ESPHome API + MQTT + GPIO KVM | Tier 2 plus physical KVM switching and owner readback | Requires wiring GPIO2/GPIO3 to the KVM |
| Zigbee · `ld2410c-h2-zigbee.yaml` | Existing Zigbee network | Zigbee | Native ESP32-H2 Zigbee presence | No WiFi/OTA; USB reflash and Zigbee pairing |

**Tier 1** is the floor: wire the radar, confirm MQTT occupancy, and prove the
board and UART pins before tuning. It intentionally has no native API and no
heartbeat or calibration logic.

**Tier 2** is the recommended default for a desk. Its `desk_seated` signal
uses measured still-energy rather than trusting close-range presence flags,
and includes the controls and heartbeat needed for dependable dormant input.

**Tier 3** adds the maintainer's physical KVM contract. dormant requests
`desktop` or `mac` over MQTT; the ESP pulses the KVM button and reads the
resulting owner. Use it only when the GPIO wiring exists.

The **Zigbee alternative** is for an ESP32-H2 and an existing Zigbee2MQTT
coordinator. It avoids WiFi and MQTT credentials on the device, but changes
require USB reflashing and usually re-pairing.
