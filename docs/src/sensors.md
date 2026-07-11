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

### Retained values and availability

**Retained messages.** dormant does not filter out MQTT retained messages — a retained publish is dispatched exactly like a live one, both on initial connect and on every reconnect. This is what gets a sensor its real state sooner after a daemon restart, instead of leaving it stuck `unavailable` until the next physical presence edge.

Caveat: MQTT retained messages carry no timestamp, so their real age is unknowable to dormant. A retained value starts the sensor's `stale_timeout` clock exactly like a fresh publish would — an old retained value is treated as "seen right now." An ancient retained `present`/`absent` therefore has up to `stale_timeout` (per-sensor override, else `stale_sensor_timeout`, default `5m`) of assumed authority before the stale-sensor sweep demotes the sensor to `unavailable`. A retained `offline` availability value does *not* get swept back once the sensor is `unavailable` — see the warning below.

**Enabling retain in Zigbee2MQTT** — the actual fix for an on-change-only sensor like the SNZB-06P: retain is a per-device setting in Z2M and defaults to *off*. Without it, there is nothing on the broker for dormant's retained-delivery support to receive.

1. Open the Zigbee2MQTT web UI → **Devices** → select the sensor.
2. Open its **Settings** tab and enable **Retain** (some Z2M versions list it under "Advanced").
   - Or set it directly over MQTT: publish to `zigbee2mqtt/bridge/request/device/options` with payload `{"id": "<friendly_name>", "options": {"retain": true}}`.
3. Restart dormant (or wait for its next broker reconnect) and confirm the sensor shows its real state without needing a fresh physical trigger.

**The three availability config keys.** By default dormant assumes the Zigbee2MQTT convention: an availability topic derived as `<topic>/availability`, with payloads `"online"`/`"offline"`. Three optional per-sensor keys override this for other conventions — for example Tasmota's LWT topic with literal `Online`/`Offline` payloads:

| Key | Type | Default | Description |
|---|---|---|---|
| `availability_topic` | string | `<topic>/availability` | Override the derived availability topic |
| `availability_payload_online` | string | `"online"` | Payload meaning the device is reachable — informational only, emits no event |
| `availability_payload_offline` | string | `"offline"` | Payload meaning the device is unreachable — emits `Unavailable` for this sensor |

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

`availability_payload_online` and `availability_payload_offline` must be non-empty and must differ from each other; when multiple sensors share the same resolved availability topic on the same broker, they must declare identical literal pairs. An availability payload that matches neither configured literal logs one warning per `(topic, sensor)` pair and is otherwise ignored — dormant never fabricates a state from a payload it can't parse.

**The `reported` hint.** Every sensor is seeded `unavailable` at daemon start (fail-safe), so a sensor that has never sent any data looks identical, at a glance, to one that just went offline. Each snapshot carries a `reported` bit — has this sensor delivered at least one event since the daemon started? The web dashboard renders a "no data since start" hint for a sensor that is `unavailable` with `reported == false`, distinguishing "never heard from since this daemon came up" from "was known, then went away." `reported` is carried across a config reload for a sensor whose own binding (topic, broker, payload literals, etc.) is unchanged; it resets to `false` if that sensor's config changed in the reload, or if it's newly added.

**Worked timeline — the residual risk to know about.** Take a single-sensor zone (no second sensor to fall back on) whose sensor is a Zigbee2MQTT device with retain now enabled, where the broker's retained occupancy value is a stale `vacant` from before a restart — while a real occupant has been sitting motionless in the room since well before that restart, with no fresh presence edge to report:

1. `t = 0` — the daemon starts, subscribes, and immediately receives the retained `vacant` value. This is a real `Absent` occupancy event to the rules engine — retained-vacant on the *occupancy* topic bypasses `unavailable_policy` entirely; it isn't an availability signal.
2. With default timings (`grace_period = 60s`, `startup_holdoff = 30s`), the grace countdown starts at `t = 0`. Because grace (60 s) outlasts holdoff (30 s), holdoff has already elapsed by the time grace expires, so nothing else gates the blank.
3. `t ≈ 60s` — grace expires and the display blanks, even though the occupant is still there, motionless. The stale retained `vacant` was never corrected by any real edge.
4. The display wakes on the occupant's very next detected movement — the wake path is never gated by grace or holdoff.

Net effect: with default timings, the screen can blank about 60 seconds after a restart despite someone being in the room, if a stale retained-vacant value was waiting on the broker. This is indistinguishable, from the occupant's side, from a false vacancy edge fired by a live sensor — dormant does not special-case it. If you tune `grace_period` *below* `startup_holdoff`, the first blank instead lands at ~holdoff, not ~grace (whichever is later wins). This is accepted as a v1 residual risk rather than built around; a per-sensor "distrust retained-absent" knob was considered and rejected for now.

> **Warning — `unavailable_policy = "absent"` with an MQTT sensor is unsafe until you verify this yourself.** Do not set a zone's `unavailable_policy = "absent"` on a zone containing an MQTT sensor unless you have personally confirmed that your Zigbee2MQTT (or other bridge) setup **republishes the sensor's occupancy state** when its device's availability recovers from `offline` back to `online`. Availability handling maps a recognized `offline` payload to `Unavailable`, gated by `unavailable_policy` — under `"absent"` that becomes a real vacancy signal and blanks the screen. If the device does not republish its own state on recovery, the sensor is **never automatically rescued**: the stale-sensor sweep only ever pushes a sensor *into* `Unavailable`, never back out of it, so only a fresh state (or availability) publish from the device can recover it. Under the default policy (`"present"`) the same stuck-`Unavailable` case is harmless — fail-safe keeps the screen on. Because this combination can strand a blanked screen indefinitely with no automatic wake path, treat it as unsafe until proven otherwise for your setup. dormant surfaces every zone/sensor pair matching this pattern as a startup- and reload-time log warning (`event = "unavailable_absent_mqtt"`) so you can grep for it — but the warning is observational only; it does not block the config from loading.

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
