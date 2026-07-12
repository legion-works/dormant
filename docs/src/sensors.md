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

For an authenticated broker, keep credentials out of `config.toml`:

```toml
# ~/.config/dormant/credentials.toml
[mqtt."tcp://localhost:1883"]
username = "dormant"
password = "secret"
```

The table key must match `sensors.<id>.broker_url` exactly.

### Retained values and availability

MQTT retained values are dispatched like live publishes on initial subscribe and reconnect. This gives an on-change sensor a state immediately after daemon restart instead of leaving it `unavailable` until the next physical edge.

MQTT retained values carry no timestamp. dormant starts the sensor's `sensors.<id>.stale_timeout` clock when it receives one, even if the retained value is old. A stale retained `present` or `absent` value can therefore remain authoritative until that timeout expires (or `daemon.stale_sensor_timeout`, default `5m`, when no per-sensor override is set).

For an on-change-only Zigbee2MQTT sensor, enable retained state on the device. Without it, the broker has no current occupancy value to send after a restart.

1. Open the Zigbee2MQTT web UI → **Devices** → select the sensor.
2. Open its **Settings** tab and enable **Retain** (some Z2M versions list it under "Advanced").
   - Or set it directly over MQTT: publish to `zigbee2mqtt/bridge/request/device/options` with payload `{"id": "<friendly_name>", "options": {"retain": true}}`.
3. Restart dormant (or wait for its next broker reconnect) and confirm the sensor shows its real state without needing a fresh physical trigger.

By default dormant derives `<topic>/availability` and expects `"online"` / `"offline"`. Override these per sensor for a different LWT convention:

| Key | Type | Default | Description |
|---|---|---|---|
| `sensors.<id>.availability_topic` | string | `<topic>/availability` | Override the derived availability topic |
| `sensors.<id>.availability_payload_online` | string | `"online"` | Payload meaning the device is reachable; informational only |
| `sensors.<id>.availability_payload_offline` | string | `"offline"` | Payload meaning the device is unreachable; emits `Unavailable` |

```toml
[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "tele/desk/SENSOR"
field = "/PIR"
availability_topic = "tele/desk/LWT"
availability_payload_online = "Online"
availability_payload_offline = "Offline"
```

The online/offline payloads must be non-empty and different. Sensors sharing one resolved availability topic on a broker must use the same payload pair. An unknown payload logs one warning per `(topic, sensor)` pair and is ignored.

Every sensor starts `unavailable`. The snapshot's `reported` diagnostic records whether that sensor has delivered any event since daemon start. The web dashboard marks `unavailable` sensors with `reported == false` as "no data since start." The bit survives reload when the sensor binding is unchanged and resets when that binding changes.

### Stale retained-vacant risk

Consider a single-sensor zone whose broker holds an old `vacant` value while someone sits motionless in the room:

1. `t = 0` — the daemon starts, subscribes, and immediately receives the retained `vacant` value. This is a real `Absent` occupancy event to the rules engine — retained-vacant on the *occupancy* topic bypasses `unavailable_policy` entirely; it isn't an availability signal.
2. With default timings (`grace_period = 60s`, `startup_holdoff = 30s`), the grace countdown starts at `t = 0`. Because grace (60 s) outlasts holdoff (30 s), holdoff has already elapsed by the time grace expires, so nothing else gates the blank.
3. `t ≈ 60s` — grace expires and the display blanks, even though the occupant is still there, motionless. The stale retained `vacant` was never corrected by any real edge.
4. The display wakes on the occupant's very next detected movement — the wake path is never gated by grace or holdoff.

With default timing, the display can blank about 60 seconds after restart despite the occupant. dormant cannot distinguish the old retained value from a fresh publish. The next detected presence edge wakes the display.

> **Do not set `zones.<id>.unavailable_policy = "absent"` for an MQTT sensor until you have verified that its bridge republishes occupancy when availability returns.** An `offline` payload maps the sensor to `Unavailable`; under the `"absent"` policy that can blank the screen. If the bridge does not republish occupancy on recovery, only a later state publish can wake it. The default `"present"` policy keeps the display on. dormant logs `unavailable_absent_mqtt` for this combination but does not reject it.

### Doctor check

```bash
dormantctl doctor mqtt
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
dormantctl doctor ha
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
dormantctl doctor usb /dev/ttyUSB0
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
