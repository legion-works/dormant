# Sensors

dormant ingests presence from three sensor types: MQTT, Home Assistant WebSocket, and USB-serial. This page covers setup and the `dormantctl doctor` checks for each.

## MQTT

dormant connects as an MQTT client (using `rumqttc`, pure Rust, no system dependency). It subscribes to one topic per sensor and reads a JSON pointer into each payload.

### Broker compatibility

Any MQTT 3.1.1 broker works: Mosquitto, EMQX, HiveMQ, Zigbee2MQTT built-in broker, Home Assistant MQTT add-on.

### Payload format

By default, dormant reads the `/occupancy` JSON pointer. For a Zigbee2MQTT sensor, the payload looks like:

```json
{
  "occupancy": true,
  "illumination": 245,
  "no_occupancy_since": 15
}
```

If your sensor publishes a different field, set `field` (e.g., `field = "/presence"`). If payloads are not JSON (raw `ON`/`OFF` text), set `payload_on = "ON"` and `payload_off = "OFF"` — these override the default JSON boolean interpretation.

### Doctor check

```bash
dormantctl doctor sensor desk
```

Verifies: broker reachability, topic subscription, payload parsing (last received value). If the check fails, verify the broker URL, topic spelling, and network connectivity.

## Home Assistant WebSocket

Connects to the HA WebSocket API and subscribes to entity state changes. Requires a long-lived access token in the credentials file.

### Setup

1. Create a long-lived access token in HA: **Settings → People → your user → Long-Lived Access Tokens**.
2. Store it in `~/.config/dormant/credentials.toml`:
   ```toml
   ha_token = "eyJ..."
   ```
3. Set file permissions:
   ```bash
   chmod 600 ~/.config/dormant/credentials.toml
   ```

### Entity types

Any binary sensor entity works: door sensors, motion sensors, mmWave presence sensors. The entity must report a state convertible to occupied/vacant. dormant interprets `"on"`/`true` as occupied and `"off"`/`false` as vacant.

### Doctor check

```bash
dormantctl doctor sensor couch
```

Verifies: WebSocket connection, authentication, entity subscription, last known state. If authentication fails (`E_HA_AUTH`), check the token and credentials file permissions. If the entity is unknown, verify the entity ID spelling and that the entity exists in HA.

## USB-serial LD2410

Reads presence data from an HLK-LD2410 mmWave radar module connected via USB-serial (CH340/CP2102 adapter).

### Hardware

- HLK-LD2410 (or B/C variant): 24 GHz, detects stationary and moving targets up to ~6 m / ~4.5 m respectively.
- USB-to-serial adapter: CH340 or CP2102, 5 V power, 3.3 V logic UART.
- Default baud: 256000, 8N1.

### Permissions

Your user must have read/write access to the serial port. On most distributions, add your user to the `dialout` group:

```bash
sudo usermod -a -G dialout $USER
# Log out and back in for the group change to take effect
```

### Doctor check

```bash
dormantctl doctor sensor radar
```

Verifies: serial port accessibility, baud rate negotiation, frame parsing, last reported state (moving/stationary targets, distance). If the port is not found (`E_SENSOR_IO`), check the device path (`ls /dev/ttyUSB*`) and group membership.

### Tuning

The LD2410 has configurable sensitivity and detection ranges. These are set via the module's own serial protocol (outside dormant's scope). Use the manufacturer's PC tool or ESPHome to configure them. Common adjustments:

- Reduce maximum detection range in small rooms (default 6 m is often too sensitive)
- Increase "no occupancy" delay if the sensor flickers (minimum 15 s on consumer modules)

## Sensor kinds

| Kind | Behavior |
|---|---|
| `"presence"` | Binary: occupied or vacant. Stable. |
| `"motion"` | Transient: triggers on motion, clears after hold time. Good for hallways and pass-through areas. |

Motion sensors use the `hold_time` to bridge gaps between motion pulses. A 5-minute hold keeps the zone occupied for 5 minutes after the last trigger — enough to prevent the TV from blanking while you are still on the couch but not moving.
