# Sensors

**What this gives you.** Three sensor inputs (MQTT, Home Assistant WebSocket,
USB-LD2410) with a uniform `[sensors.<id>]` config shape and a uniform doctor
probe per type.

**When to use it.** Wiring the first presence source into a config. Choose
MQTT for an ESP32 or Zigbee sensor, HA WebSocket for a sensor already in Home
Assistant, or USB for a direct-attached HLK-LD2410 radar.

**Quick setup.** Pick the `type`, drop in one block, hot-reload:

```toml
[sensors.desk]
type = "mqtt"
broker_url = "tcp://localhost:1883"
topic = "ld2410c-desk/binary_sensor/desk_presence_desk_seated/state"
```

```bash
dormantctl reload && dormantctl status
```

The sensor appears in `dormantctl status` as `unavailable` until its first
event — the default zone policy treats unavailable as present, so the display
stays on.

---

dormant ingests presence from three sensor types: MQTT, Home Assistant WebSocket, and USB-serial. This page covers setup and the `dormantctl doctor` checks for each.

## Two ways to wire the LD2410C

The HLK-LD2410C mmWave radar is a plain serial device, so the same sensor reaches dormant through either of two topologies — pick per deployment; both are one config block apart.

| Topology | dormant `type` | Path | When it wins |
| --- | --- | --- | --- |
| **Radar on an ESP32 → WiFi → MQTT** | `mqtt` | Radar → ESP32 (ESPHome) → WiFi → broker → dormant | Sensor placed anywhere with power; tuned live from Home Assistant; one radar can feed many consumers. Appears in HA natively over the ESPHome API. |
| **Radar direct to the host** | `usb-ld2410` | Radar → USB-serial adapter → dormant | Sensor at the machine it guards: no WiFi, no broker, lowest latency. The ESP is not needed; dormant reads the raw UART frames itself. |

A ready-to-flash ESPHome config for the MQTT topology ships at [`examples/esphome/ld2410c-c6-wifi-mqtt.yaml`](https://github.com/legion-works/dormant/blob/dev/examples/esphome/ld2410c-c6-wifi-mqtt.yaml) — it exposes near/far distance zones, per-gate sensitivity thresholds (g0–g8 move/still), an energy-gated `desk_seated` template that bypasses the LD2410C's presence-flag quirks, and a tunable still-energy floor, all adjustable from HA without reflashing. The USB topology needs only a `type = "usb-ld2410"` sensor block (see below) and a ~$2 adapter.

Both are **fail-safe symmetric**: broker loss (MQTT, via the retained LWT) and USB unplug both mark the sensor `unavailable`, which the default zone policy treats as *present* — dormant never blanks a room it can't see, whichever transport drops.

> **2D zones need different silicon.** The LD2410C reports one target along a single axis (distance), so it does 1D near/far zones only. True X/Y polygon zones (as on the Everything Presence Pro) need a multi-target radar like the LD2450, which wires to the same ESP32 identically — swap the module, keep the board and config shape.

### LD2410C tuning and hardware quirks

The LD2410C divides its detection space into 9 gates. Each defaults to a 75 cm radial slice, and each gate has independent **moving** and **static** energy thresholds (0–100; lower = more sensitive). A target is reported only when the measured energy exceeds that gate's threshold.

| Gate | Approx. range | Default move | Default still | Notes |
| --- | --- | --- | --- | --- |
| 0 | 0–75 cm | 50 | 0 | Still threshold **not hardware-settable** per HLK datasheet; any value written is ignored. |
| 1 | 75–150 cm | 50 | 0 | Still threshold **not hardware-settable**, same as gate 0. |
| 2 | 150–225 cm | 40 | 40 | |
| 3 | 225–300 cm | 30 | 40 | |
| 4 | 300–375 cm | 20 | 30 | |
| 5 | 375–450 cm | 15 | 30 | |
| 6 | 450–525 cm | 15 | 20 | |
| 7 | 525–600 cm | 15 | 20 | |
| 8 | 600–675 cm | 15 | 20 | |

**Gate 0/1 limitation.** A person sitting still within ~150 cm of the sensor cannot be detected as a *still* target — gate 0 and 1 have no settable static sensitivity, and their moving thresholds (50) are the highest of any gate. A desk-mounted sensor is therefore the worst case: the occupant sits in the gates with no static detection and the strictest moving threshold.

**Mounting guidance.** Position the sensor so the monitored position falls in gates 2–5 (150 cm to 450 cm), where both moving and static thresholds are available and defaults are already lower. If the sensor must be close to the occupant, lower the gate 0/1 **moving** thresholds and use the energy-gated `desk_seated` template (in the example YAML) instead of the LD2410C's native presence flags — the template keys off the raw still-energy level, which rises reliably even when the flags stay absent.

**Re-arm quirk.** Multiple HA-community reports describe the LD2410C still-presence flag requiring a *moving* trigger to re-latch after it clears — still-energy above the gate threshold alone does not always re-arm `has_still_target`. The energy-gated `desk_seated` template avoids this flag and reads the energy level directly, so it is immune to the re-arm quirk.

**Staleness.** Binary sensors publish occupancy on-change only, so a long still period (occupant seated, no movement) silences the state topic until dormant's `sensors.<id>.stale_timeout` fires and the sensor reads unavailable. The fail-safe (default zone policy = `"present"`) keeps the display on, but occupancy-driven wake-on-return is lost until the next MQTT edge. The example YAML therefore ships a 5-minute heartbeat: an `interval` block re-publishes the current `desk_seated` state (retained) even when unchanged, so topic silence again means "device gone" rather than "occupant still". Set `stale_timeout` to a bit over one heartbeat (e.g. `"6m"`) and the sensor only goes unavailable when the ESP genuinely drops off the network. When the sensor's broker publishes a retain-able LWT (see [Retained values and availability](#retained-values-and-availability) below), an explicit `online` availability frame outranks topic silence: a sensor with a healthy LWT stays `Present` across minutes of unchanged occupancy, and only goes `Unavailable` on `offline` / broker failure. The `stale_timeout` then becomes a backstop for sensors without an LWT topic.

Setting per-gate thresholds is necessary but **not sufficient** to fix close-range seated detection on its own — the gate 0/1 hardware limit and the re-arm quirk remain. The energy-gated template is the primary mitigation; per-gate thresholds fine-tune the radar for the gate range where the occupant actually sits.

**Calibrating the floor and gates.** The most reliable tuning method is a marked-scenario data run: record still-energy values for each scenario (away, seated, standing, couch, walking) with the sensor in its final position. Compare the per-scenario distributions and set the still-energy floor between the worst-seated-still measurement and the best-absent measurement, leaving margin on both sides. When a couch or sofa sits behind the desk, prefer cutting the max still gate over raising the floor — stopping the radar from reporting targets beyond the desk in-hardware prevents false presence at the source and stabilizes the reported detection distance against far-wall echoes.

**Switching an existing sensor over to `desk_seated`.** The template publishes on its
own MQTT topic, so an existing `[sensors.*]` block keeps reading the old
distance-gated entity until you repoint it. After reflashing, confirm the new
topic is live and then update the config:

```bash
# 1. confirm the ESP is publishing the new entity
mosquitto_sub -h <broker> -v -t 'ld2410c-desk/binary_sensor/#' -W 5

# 2. point the sensor at it (topic slug follows the entity name)
#    topic = "ld2410c-desk/binary_sensor/desk_presence_desk_seated/state"

# 3. hot-reload — no daemon restart needed
dormantctl reload && dormantctl status
```

Verify the slug from the `mosquitto_sub` output rather than assuming it: ESPHome
derives it from the entity name, so a renamed template changes the topic.
Keeping the old `desk_near` subscription is also valid — both entities publish
independently, and `desk_near` remains the better signal for the far zone.

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
| `sensors.<id>.availability_payload_online` | string | `"online"` | Payload meaning the device is reachable |
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

**How availability gates the stale timeout (issue #136).** A matching
`online` payload is the source's reachability claim: dormant records it in
the rules engine's `availability_online` set, and the stale-sensor sweep
leaves the sensor alone on state-topic silence. This lets a seated
occupant (no motion, no occupancy re-publish) keep the screen on for
minutes without a fresh state edge. The claim is deliberately NOT a
presence refresh — `last_seen` still advances on real wall-clock time,
so a later broker failure is detectable. A matching `offline` payload
(LWT) immediately demotes the sensor to `Unavailable` and clears the
claim, and a broker disconnect emits `Unavailable` for every owned
sensor (which also clears the claim), so a dead connection can never
preserve stale presence forever. Sensors WITHOUT an availability topic
keep the pre-#136 behavior exactly: silence past `stale_timeout` marks
them `Unavailable`. Keep the 6-minute `stale_timeout` guidance for
sensors whose bridge does not publish LWT.

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

The LD2410 has configurable sensitivity and detection ranges. See the [LD2410C tuning and hardware quirks](#ld2410c-tuning-and-hardware-quirks) section above for the gate table, the gate 0/1 static-sensitivity limitation, mounting guidance, and the re-arm quirk — those apply regardless of transport.

For the USB-serial topology, thresholds are set via the module's own serial protocol (outside dormant's scope). Use the manufacturer's PC tool or ESPHome to configure them. Common adjustments:

- Reduce maximum detection range in small rooms (default 6 m is often too sensitive)
- Increase "no occupancy" delay if the sensor flickers (minimum 15 s on consumer modules)

## Sensor kinds

| Kind | Behavior |
|---|---|
| `"presence"` | Binary: occupied or vacant. Stable. |
| `"motion"` | Transient: triggers on motion, clears after hold time. Good for hallways and pass-through areas. |

Motion sensors use the `hold_time` to bridge gaps between motion pulses. A 5-minute hold keeps the zone occupied for 5 minutes after the last trigger — enough to prevent the TV from blanking while you are still on the couch but not moving.
