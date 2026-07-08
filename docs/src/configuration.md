# Configuration

dormant configuration is a TOML file. Optional keys list their defaults here; required keys are marked required. Unknown keys are rejected at startup (strict mode) or warned about (warn mode — default).

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

If your broker requires authentication, add a `[mqtt]` section to your credentials file:

```toml
# credentials.toml
[mqtt."tcp://mqtt.local:1883"]
username = "your-username"
password = "your-password"
```

The key MUST match the sensor's `broker_url` **exactly** — a `mqtt://` vs `tcp://` mismatch or trailing difference causes a silent miss (anonymous connect → auth failure).

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

### Manual-only displays

A display in `[displays]` referenced by **no rule** is **manual-only**: the
daemon builds it and it responds to `dormantctl blank` / `dormantctl wake`, the
web UI, and the tray app, but no zone or rule ever auto-blanks or auto-wakes it.

A `ladder` on a rule-less display is rejected at validation time with error
`E_CONFIG_INVALID` — a ladder is an auto-escalation that requires a rule to
drive it.  Use `blank_mode` (or `blank_mode` + `degraded_mode`) for
manual-only displays.

Manual-only phase survives a config reload: if you blanked a manual-only
display via `dormantctl blank` and then edit the config, it stays blanked
(not defensive-woken).  Across a full daemon **restart** (not reload) there
is no persisted state, so a manual-only display starts `active`.

Example — a Samsung Tizen TV controlled by hand:

```toml
[displays.tv]
controllers = ["samsung-tizen"]
blank_mode = "screen_off_audio_on"
host = "192.168.1.50"
```

### Escalation ladder

Instead of `blank_mode` and `degraded_mode`, a display can define an ordered
**ladder** of stages. Each stage is tried in order; if one fails, the next one
runs. A stage with a `dwell` duration ("dwell stage") is held for that long
before the ladder advances. The last stage (the terminal stage) has no time
limit — the ladder stays there until an external event (sensor, user wake)
moves it.

`blank_mode` and `ladder` are mutually exclusive — a config containing both
is rejected at startup.

#### Ladder form (array of tables)

```toml
[displays.main]
controllers = ["ddcci", "kwin-dpms"]
output = "DP-1"
ladder = [
  { kind = "render_black", dwell = "30s" },   # audio-safe black overlay
  { kind = "power_off" },                       # terminal: true panel-off
]
```

#### Stage kinds

| Kind | Description |
|------|-------------|
| `power_off` | Controller power-off mode |
| `screen_off_audio_on` | Controller screen-off-audio-on mode |
| `brightness_zero` | Controller brightness-zero mode |
| `render_black` | Software fullscreen black overlay (render backend) |
| `render_screensaver` | Software screensaver (render backend; requires `screensaver` config) |

#### `dwell`

An optional [humantime](https://docs.rs/humantime/latest/humantime/) duration
(e.g., `"30s"`, `"5m"`) the ladder stays at this stage before advancing.
Omitting `dwell` (or setting it to `None`) makes the stage terminal — the
ladder stops here and waits for an external event.

Every non-terminal stage **must** have a `dwell`; validation rejects ladders
that would skip past a dwell-less intermediate stage.

#### Render stages

`render_black` and `render_screensaver` use the software render backend, which
is **off by default**. Build dormant with `--features render` to enable it;
configs containing a render stage are rejected at startup with error
`E_RENDER_UNAVAILABLE` when the feature is absent.

Render stages require:

1. **`output` set** on the display (the Wayland output connector name, e.g. `"DP-1"`).
2. **At least one local controller** (`kwin-dpms`, `ddcci`, or `command`) in
   the display's `controllers` list — render stages cannot run on a
   remote-only display (`samsung-tizen` / `ha-passthrough` alone).

`render_screensaver` additionally requires a `[displays.<id>.screensaver]`
section with at least one image source (a `path` or `urls`).

##### Screensaver configuration

```toml
[displays.my-display.screensaver]
trigger = "vacancy"     # only value supported
audio = false           # default: false (muted)

[[displays.my-display.screensaver.source]]
path = "/home/user/Pictures/screensaver"
recurse = false         # default: false
shuffle = true          # mutually exclusive with `order`
image_duration = "8s"   # per-item duration override

[[displays.my-display.screensaver.source]]
urls = ["https://example.com/background.jpg"]
order = "sequential"    # only value accepted; mutually exclusive with `shuffle`
```

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `trigger` | string | no | `"vacancy"` | Trigger for the screensaver. Only `"vacancy"` is supported (the ladder itself is vacancy-driven). |
| `audio` | boolean | no | `false` | Whether audio playback is enabled. `false` mutes the player at init. |
| `scale_mode` | string | no | `"fill"` | How source frames are scaled onto the output rectangle. One of `"fill"`, `"fit"`, `"stretch"`, `"center"`. See [Scale mode](#scale-mode) below. |
| `transition` | string | no | `"crossfade"` | How consecutive playlist items transition. `"crossfade"` blends successive frames with a per-pixel u8 lerp; `"none"` cuts immediately (the pre-feature behaviour). |
| `transition_duration` | duration | no | `"1s"` | Length of the crossfade blend (ignored when `transition = "none"`). Bounded to `100ms..=10s`. |
| `[[…source]]` | array | yes | — | Ordered list of media sources for the playlist. |

Each source supports:

| Key | Type | Required | Default | Description |
|-----|------|----------|---------|-------------|
| `path` | string | conditional | — | Local directory of images / video files. Mutually exclusive with `urls`. |
| `urls` | []string | conditional | `[]` | Remote URLs. Mutually exclusive with `path`. |
| `recurse` | boolean | no | `false` | Scan `path` recursively for media files. |
| `shuffle` | boolean | no | `false` | Shuffle items from this source (Fisher-Yates, seeded per restart). Mutually exclusive with `order`. |
| `order` | string | no | — | Ordering strategy. Only `"sequential"` is accepted. Mutually exclusive with `shuffle`. |
| `image_duration` | duration | no | 10 s | Per-image display duration override (must be > 0). |

###### Transitions

When `transition = "crossfade"` (the default), successive playlist items
blend with a per-pixel u8 lerp over `transition_duration`.  The blend is
driven by a calloop timer on the Wayland thread; measured blend cost
is ≈0.9 ms/frame at 3072×1728 — negligible against any reasonable
frame budget.  Set `transition_duration` between `100ms` and `10s`;
the validator rejects anything outside that range.

`transition = "none"` keeps the legacy hard-cut behaviour (byte-identical
to pre-M3) and is useful for benchmarks or operators who prefer the
instantaneous switch.  When `transition = "none"`, the `transition_duration`
field is ignored.

**Playlist assembly:** The playlist is built at startup and on every config
reload — file-system scanning runs off the Wayland thread.  Changes to the
source directories or `screensaver` config require a reload (`SIGHUP` or
`dormantctl reload`) to take effect.

**Feature gate:** `render_screensaver` requires the `render` build feature.

To use the screensaver, the display's ladder must include a `render_screensaver`
stage, typically as the terminal stage after controller attempts have failed:

```toml
[displays.my-display]
controllers = ["ddcci", "command"]
output = "DP-1"
ladder = [
  { kind = "power_off", dwell = "30s" },
  { kind = "render_black", dwell = "5m" },
  { kind = "render_screensaver" },
]
```

###### Scale mode

The `scale_mode` key controls how source frames are mapped onto the rendered
output rectangle.  Four modes are recognised; the default is `fill`, matching
the OS-screensaver norm (no black bars, regardless of source aspect ratio).

| Mode | What you see | mpv mapping |
|------|--------------|-------------|
| `"fill"` (default) | Crop-to-fill: the source is zoomed so it covers the entire output rectangle; off-axis is cropped. **No black bars.** | `panscan=1.0`, `keepaspect=yes` |
| `"fit"` | Aspect-fit letterbox: the source is scaled to fit inside the output rectangle while preserving its aspect ratio; black bars fill the gap. **This was the legacy behaviour** before `scale_mode` was added. | `keepaspect=yes`, `panscan=0.0` |
| `"stretch"` | Stretch: the source is scaled to exactly fill the output rectangle, distorting aspect ratio. **No black bars**, but proportions may look wrong; useful only when source aspect matches the display. | `keepaspect=no` |
| `"center"` | 1:1 centre: the source is shown at native pixel dimensions (no scaling), centred in the output rectangle. Black bars fill the gap. | `video-unscaled=yes`, `keepaspect=yes` |

Validation rejects any unknown value with an `E_SCREENSAVER_SOURCE`-class
error naming the allowed set.  The four modes were empirically verified to
take effect under `MPV_RENDER_API_TYPE_SW` (the libmpv SW render context
used by the screensaver) — the property values flow through to mpv and
influence the scaling at frame-blit time.

Validation rejects:

- A `render_screensaver` stage without a `screensaver` section.
- A `screensaver` section with no sources, or sources with neither `path` nor `urls`.
- A source with both `path` and `urls` set.
- A source with both `shuffle` and `order` set.
- An `order` value other than `"sequential"`.
- A `trigger` value other than `"vacancy"`.
- A `scale_mode` value other than `"fill"`, `"fit"`, `"stretch"`, or `"center"`.
- An `image_duration` of zero.

#### Backward compatibility

Configs that use `blank_mode` (and optionally `degraded_mode`) — the pre-ladder
style — continue to work unchanged. Internally, `blank_mode` is desugared to a
single-stage ladder (`{ kind = "<blank_mode>" }`) with no dwell.

`blank_mode` + `ladder` together is rejected. `degraded_mode` + `ladder` is
also rejected (the ladder itself chains fallbacks).

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

## Config editor (web UI)

When the web UI is enabled (see [Web UI](web-ui.md)), the **Settings** tab on the Config page provides a form-based editor for live config changes without editing the TOML file directly.

**Editable fields (v1):** leaf values (strings, numbers, durations), whole arrays (ladders, rule display lists, screensaver source lists), and a limited set of optional keys can be removed. **Not editable:** `type`, `blank_data`, `wake_data` (hard-locked), any entity table add/remove (file-only), and any field whose value carries redacted credentials. Display command strings, controller lists, and mode lists are file-only in v1 (the Settings form does not render controls for them).

**Backups:** every apply creates a timestamped backup of the previous config in `<config-dir>/backups/config.toml.<rfc3339>.<rand>`, keeping at most 5 newest copies. The directory is created with mode `0o700`.

See the [Web UI](web-ui.md#config) page for the full editor workflow, outcome banners, conflict handling, and unsaved-changes guard.

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
controllers = ["ha-passthrough"]
blank_mode = "screen_off_audio_on"
ha_url = "http://ha.local:8123"
blank_service = "media_player.turn_off"
blank_data = { entity_id = "media_player.living_tv" }
wake_service = "media_player.turn_on"
wake_data = { entity_id = "media_player.living_tv" }
modes = ["screen_off_audio_on"]
# Note: samsung-tizen (native WebSocket KEY_PICTURE_OFF) will replace this
# once hardware verification completes — ha-passthrough via the HA Samsung
# integration is the recommended path today.

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
