# Configuration

dormant configuration is a TOML file. Every key has a documented default. Unknown keys are rejected at startup (strict mode) or warned about (warn mode — default).

## Schema version

```toml
config_version = 1
```

Required. Must be `1`. This gates backward compatibility — bump when making breaking schema changes.

## `[daemon]` — daemon-level settings

| Key | Type | Default | Description |
|---|---|---|---|
| `startup_holdoff` | duration | `"30s"` | Wait after startup before acting (allows sensors to stabilize) |
| `stale_sensor_timeout` | duration | `"5m"` | How long a sensor can go silent before considered stale |
| `log_level` | string | `"info"` | Tracing log level: `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"` |
| `socket_path` | path | auto | Unix-domain socket for `dormantctl` IPC (defaults to XDG_RUNTIME_DIR) |
| `idle_time_unit` | string | `"auto"` | How to interpret screensaver idle values: `"auto"`, `"ms"`, `"s"`. KDE returns ms despite the spec saying seconds — auto detects the unit at runtime |
| `reload_debounce` | duration | `"500ms"` | Coalesces rapid config-file changes into a single reload |

## `[sensors.<id>]` — sensor definitions

Each sensor has a user-chosen `id` (e.g., `desk`). The `type` field selects the source.

Common fields on all sensor types:

| Key | Type | Default | Description |
|---|---|---|---|
| `kind` | string | `"presence"` | Sensor semantics: `"presence"` (binary occupied/vacant) or `"motion"` (transient event) |
| `hold_time` | duration | none | Per-sensor override: how long occupancy persists after the last trigger |
| `stale_timeout` | duration | none | Per-sensor override: how long before no data means unavailable |

### `type = "mqtt"`

Connects to an MQTT broker and subscribes to a topic.

| Key | Type | Required | Description |
|---|---|---|---|
| `broker_url` | string | yes | MQTT broker URL (e.g., `tcp://localhost:1883`) |
| `topic` | string | yes | MQTT topic to subscribe to |
| `field` | string | `"/occupancy"` | JSON pointer into the payload (RFC 6901) |
| `payload_on` | string | none | Override for the "on" payload value (default: JSON `true`) |
| `payload_off` | string | none | Override for the "off" payload value (default: JSON `false`) |

Example:

```toml
[sensors.desk]
type = "mqtt"
broker_url = "tcp://mqtt.local:1883"
topic = "zigbee2mqtt/desk-sensor"
```

### `type = "ha"`

Connects to Home Assistant via WebSocket and subscribes to an entity state.

| Key | Type | Required | Description |
|---|---|---|---|
| `url` | string | yes | HA WebSocket URL (e.g., `ws://ha.local:8123/api/websocket`) |
| `entity` | string | yes | Entity ID to track (e.g., `binary_sensor.couch_presence`) |

The HA long-lived access token goes in the credentials file, not in the main config:

```toml
# credentials.toml
ha_token = "eyJ..."
```

### `type = "usb-ld2410"`

Reads presence data from an HLK-LD2410 mmWave radar module over USB-serial.

| Key | Type | Required | Description |
|---|---|---|---|
| `port` | string | yes | Serial port path (e.g., `/dev/ttyUSB0`) |
| `baud` | integer | `256000` | Baud rate |

Example:

```toml
[sensors.radar]
type = "usb-ld2410"
port = "/dev/ttyUSB0"
```

## `[zones.<id>]` — zone definitions

A zone fuses one or more sensor references into a single presence signal. Members can be sensor IDs or nested zones (`"zone:<id>"`).

| Key | Type | Required | Description |
|---|---|---|---|
| `mode` | string | yes | Fusion mode: `"any"`, `"all"`, `"quorum"`, `"weighted"` |
| `members` | []string | yes | Sensor/zone IDs. Prefix `"zone:"` for nested zones |
| `quorum` | integer | conditional | Required member count for `"quorum"` mode |
| `threshold` | float | conditional | Threshold fraction (0.0–1.0) for `"weighted"` mode |
| `weights` | table | optional | Per-member float weights for `"weighted"` mode |
| `unavailable_policy` | string | `"present"` | How to treat unavailable members: `"present"` (fail-safe) or `"absent"` |

### Zone modes

- **`any`** — occupied if any member reports occupied.
- **`all`** — occupied only if every member reports occupied.
- **`quorum`** — occupied if at least N members report occupied (requires `quorum` key).
- **`weighted`** — occupied if the weighted sum of occupied members meets a threshold (requires `threshold` and `weights` keys).

### Unavailable policy

When a sensor stops reporting (broker down, USB unplugged), it becomes *unavailable*. The zone's `unavailable_policy` decides what to do:

- **`present`** (default, fail-safe) — treat unavailable sensors as occupied. The room is never blanked unless *all* sensors confirm vacancy.
- **`absent`** — treat unavailable sensors as vacant. Only use this when you are certain a sensor failure is acceptable — a sensor that goes offline will trigger a blank.

## `[displays.<id>]` — display definitions

Each display has a user-chosen `id`. The `controllers` list is an ordered fallback chain.

| Key | Type | Required | Description |
|---|---|---|---|
| `controllers` | []string | yes | Ordered list of controller names to try |
| `blank_mode` | string | yes | Primary blank mode: `"screen_off_audio_on"`, `"power_off"`, `"brightness_zero"` |
| `degraded_mode` | string | no | Fallback mode if primary is unsupported |
| `output` | string | conditional | KWin output name (e.g., `"DP-1"`) |
| `ddc_display` | string | conditional | DDC/CI display identifier |
| `host` | string | conditional | Hostname/IP for network-controllable displays |
| `wol_mac` | string | conditional | MAC address for Wake-on-LAN |
| `blank_command` | string | conditional | Shell command to blank (for `"command"` controller) |
| `wake_command` | string | conditional | Shell command to wake (for `"command"` controller) |
| `modes` | []string | conditional | Supported modes for `"command"` or `"ha-passthrough"` |
| `ha_url` | string | conditional | HA URL for `"ha-passthrough"` |
| `blank_service` | string | conditional | HA service to call for blanking |
| `blank_data` | any | conditional | HA service data for blanking (TOML value) |
| `wake_service` | string | conditional | HA service to call for waking |
| `wake_data` | any | conditional | HA service data for waking (TOML value) |
| `command_timeout` | duration | `"10s"` | Timeout for a single blank/wake command |
| `restore_brightness` | integer | `80` | Brightness level to restore on wake (0–100) |
| `treat_unreachable_as_blanked` | boolean | `true` | If controller is unreachable, assume display is blanked (fail-safe) |

## `[rules.<id>]` — rule definitions

A rule links a zone to one or more displays with timing parameters.

| Key | Type | Required | Description |
|---|---|---|---|
| `zone` | string | yes | Zone ID whose state drives this rule |
| `displays` | []string | yes | Display IDs to control |
| `grace_period` | duration | `"60s"` | Zone must be stable for this long before acting |
| `min_blank_time` | duration | `"10s"` | Minimum time a display stays blanked before waking |
| `min_wake_time` | duration | `"10s"` | Minimum time a display stays awake before blanking |
| `inhibitors` | []string | `[]` | Named inhibitors that suppress this rule: `"user-activity"`, `"manual-pause"` |
| `activity_idle_threshold` | duration | `"2m"` | How long without input before user-activity inhibitor considers user idle |
| `activity_poll_interval` | duration | `"5s"` | How often to poll activity state |
| `wake_retries` | integer | `3` | Number of wake retries before escalating |
| `wake_retry_backoff` | duration | `"2s"` | Backoff before the first wake retry |
| `wake_retry_interval` | duration | `"60s"` | Interval between successive wake retries |

## `[credentials]` — credentials file

Stored in a separate file (`credentials.toml`) with `600` permissions.

| Key | Type | Description |
|---|---|---|
| `ha_token` | string | Home Assistant long-lived access token |
| `samsung.<host>` | string | Samsung TV token, one per host key |

Example:

```toml
ha_token = "eyJ..."

[samsung]
"192.168.1.50" = "eyJ..."
```

## Cookbook: multi-zone household

Desk monitor blanks when you leave the room. Living room TV blanks when no motion for 5 minutes. Kitchen display stays awake while any movement in the open-plan area. Hallway light-weight sensor provides backup.

```toml
config_version = 1

# ── Sensors ──

[sensors.desk_radar]
type = "usb-ld2410"
port = "/dev/ttyUSB0"

[sensors.living_motion]
type = "mqtt"
broker_url = "tcp://mqtt.local:1883"
topic = "zigbee2mqtt/living-room-sensor"
hold_time = "5m"
kind = "motion"

[sensors.kitchen_presence]
type = "mqtt"
broker_url = "tcp://mqtt.local:1883"
topic = "zigbee2mqtt/kitchen-sensor"
kind = "presence"

[sensors.hallway]
type = "ha"
url = "ws://ha.local:8123/api/websocket"
entity = "binary_sensor.hallway_occupancy"

# ── Zones ──

[zones.desk]
mode = "any"
members = ["desk_radar"]

[zones.living_room]
mode = "all"
members = ["living_motion"]

[zones.open_plan]
mode = "any"
# Treat hallway as backup — if kitchen sensor goes stale, still don't blank
members = ["kitchen_presence", "hallway"]

# ── Displays ──

[displays.desk_monitor]
controllers = ["ddcci"]
blank_mode = "power_off"

[displays.living_tv]
controllers = ["samsung-tizen"]
blank_mode = "screen_off_audio_on"
host = "192.168.1.50"

[displays.kitchen_display]
controllers = ["command"]
blank_mode = "power_off"
blank_command = "/usr/bin/xset dpms force off"
wake_command = "/usr/bin/xset dpms force on"
modes = ["power_off"]

# ── Rules ──

[rules.desk_blank]
zone = "desk"
displays = ["desk_monitor"]
grace_period = "30s"
inhibitors = ["user-activity", "manual-pause"]

[rules.living_blank]
zone = "living_room"
displays = ["living_tv"]
grace_period = "5m"
min_blank_time = "60s"

[rules.kitchen_blank]
zone = "open_plan"
displays = ["kitchen_display"]
grace_period = "2m"
```
