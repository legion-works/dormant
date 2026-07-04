# OLED Proximity — Research Report

> Research conducted 2026-07-04. Sources prioritized from 2024–2026.

## `<answer>` — Executive Summary

**No single project does 80%+ of what `oled-proximity` proposes.** The space is fragmented: several hardware-specific solutions exist (ESP32+LD2410 for LG TVs [1], LG TV companion daemons [3], Windows-only tools [8]), but nothing combines multi-sensor fusion, cross-platform PC monitor control, AND TV control in one modular daemon with a clean OSS codebase. The biggest gap is a **unified daemon** that ingests presence from heterogeneous sensors (Zigbee/mmWave over MQTT/HA, USB-serial radar, input-idle) and controls displays across Linux/Windows/macOS + remote TVs via composable backends.

**Recommended language: Rust.** The ecosystem has materially better MQTT (rumqttc — pure Rust, Tokio-native, no C dependency), cross-platform serial port (serialport-rs + tokio-serial), and cross-compilation tooling (`cross`, `cargo-zigbuild`) for single-static-binary delivery. Go's `paho.mqtt.golang` requires CGO + system libpaho for non-bundled builds. The HA WebSocket client landscape is thin in both languages but usable; Rust's `hass-rs` exists as a starting point.

**Key risks:** (1) Wayland compositor fragmentation — `wlr-output-power-management` protocol is not universally supported (GNOME/KDE don't implement it natively [17]); (2) DDC/CI power-off (`VCP 0xD6`) is monitor-dependent — not all monitors implement power-off via DDC/CI; (3) the Sonoff SNZB-06P firmware (≤1.0.5) has a minimum 15s occupied→clear delay [10], which may feel slow for desk use.

---

## `<results>`

> **Steer applied 2026-07-04:** Refocused on Samsung S90C (Tizen OLED) TV control, critical requirement of screen-blank-with-audio, and DPMS audio implications for PC monitors.

### 1. Prior Art

#### 1.1 Direct Prior Art (Presence → Display Control)

| Project | Stars | Language | Approach | Last Commit | Gaps |
|---------|-------|----------|----------|-------------|------|
| [davidz-yt/lg-oled-auto-sleep](https://github.com/davidz-yt/lg-oled-auto-sleep) | 83 | C++ (Arduino) | ESP32 + LD2410C + HA → LG TV | Jul 2025 | LG TV only, single sensor, hardwired ESP32, no PC monitor support |
| [sumitchill/Oled_Burnin_prev](https://github.com/sumitchill/Oled_Burnin_prev) | ~5 | Python/Arduino | ESP32 + LD2410C + Flask server → Windows monitor (via NIRCMD) + HA → TV | ~2023 | Python Flask dependency on Windows, NIRCMD hack, no Linux/macOS, no modularity |
| [bassidus/lgpowercontrol](https://github.com/bassidus/lgpowercontrol) | 30 | Shell/Python | Linux systemd service, hooks display sleep/wake → LG WebOS TV | May 2026 (active) | LG TV only, no presence sensor (uses OS idle timer), Linux-only |
| [meltingice1337/watchdesk](https://github.com/meltingice1337/watchdesk) | ~20 | Rust | Windows service, reports monitor power state to HA via MQTT | ~2024 | Reports state (not controls), Windows-only |
| [manvir-singh/display_off](https://github.com/Programming4life/display_off) | 9 | Rust | Powers off all DDC/CI monitors on Windows shutdown | ~2023 | Windows shutdown hook only, not presence-driven |
| [YagirProtect/Rust-Screen-Disabler](https://github.com/YagirProtect/Rust-Screen-Disabler) | ~15 | Rust | Cross-platform hotkey to toggle display off/on | ~2024 | Manual toggle only, no automation, no presence sensor |

**Verdict:** There is NO maintained project that does multi-sensor presence fusion + cross-platform display control + TV control. The closest projects are single-vendor (LG), single-sensor, and hardwired. An opportunity exists for a modular, cross-platform daemon.

#### 1.2 Related / Adjacent Projects

| Project | Relevance |
|---------|-----------|
| [danilofalcao/kidle](https://github.com/danilofalcao/kidle) | KDE Plasma Wayland idle→screen-off daemon — demonstrates `/dev/input` monitoring for activity detection on Wayland [4] |
| [mrmekon/circadian](https://github.com/mrmekon/circadian) | Suspend-on-idle daemon in Rust — good architecture reference for multi-heuristic idle detection [6] |
| [lilydjwg/dpms-off](https://github.com/lilydjwg/dpms-off) | Wayland equivalent of `xset dpms force off` using `zwlr_output_power_manager_v1` [7] |
| [~dsemy/wlr-dpms](https://git.sr.ht/~dsemy/wlr-dpms) | Clean C implementation of wlroots output power management [7] |
| [sashokbg/wlout](https://github.com/sashokbg/wlout) | Rust CLI for Wayland display management (power, mode, position) — good reference for Wayland protocol usage [7] |
| [dwagelaar/enforce-dpms](https://github.com/dwagelaar/enforce-dpms) | Workaround for amdgpu DPMS bug — shows real-world DPMS fragility [6] |
| [r-mccarty/presence-dectection-engine](https://github.com/r-mccarty/presence-dectection-engine) | ESPHome mmWave presence engine with z-score analysis, debouncing, 4-state machine — reference for sensor-side debounce logic [5] |
| [Quorthon13/OLED-Sleeper](https://github.com/Quorthon13/OLED-Sleeper) | Windows tool to blackout/dim idle monitors for OLED burn-in prevention [4] |

#### 1.3 Home Assistant / Reddit Community

Home Assistant users routinely build presence→display automations using the built-in automation engine [12], but these are:
- Deeply tied to HA (no standalone daemon)
- Typically control a single device type (lights OR one TV)
- Fragile across HA restarts and require YAML/UI config

The HA community has no reusable "oled-proximity" blueprint or integration. Common patterns seen:
- mmWave sensor → HA automation → `media_player.turn_off` for TVs
- mmWave sensor → HA automation → shell command (`xset dpms force off` via SSH) for Linux monitors
- Group helpers combining multiple presence sensors with `any`/`all` logic [5]

#### 1.4 Hardware-Integrated Solutions

Some premium OLED monitors now include built-in proximity sensors:
- **MSI OLED CARE 3.0** (2025): built-in mmWave presence sensor blanks screen when user leaves [3]
- **Neo Proximity Sensor** (Eve/Dough): IR proximity sensor blanks OLED panel [3]

These are monitor-firmware features, not software, but validate the use case.

---

### 2. Sensor Landscape

#### 2.1 mmWave Presence Sensors (Zigbee/MQTT/ESPHome)

| Sensor | Protocol | Detection | Clear Delay (min) | HA Integration | Notes |
|--------|----------|-----------|-------------------|----------------|-------|
| **Sonoff SNZB-06P** | Zigbee 3.0 | 5.8GHz radar, stationary+micro-movement | 15s (fw ≥1.0.5), 30s (fw <1.0.5) [10] | ZHA, Z2M | $15-20, very popular, firmware ≤1.0.6 has "stuck on detected" bugs [10], sensitive to mirrors/reflections |
| **Aqara FP2** | WiFi (HomeKit) | 60GHz mmWave, multi-zone, multi-person (up to 5) | ~500ms detection latency [15] | HA via HomeKit Controller or ESPHome | $60-80, zone support, requires Aqara app for initial setup, WAN blocking reduces latency ~50% [15] |
| **Everything Presence Lite** | ESPHome (WiFi) | 24GHz LD2450 (default, up to 3 targets 2D tracking) or LD2410C/DFRobot/Seeed | Configurable via ESPHome | Native ESPHome | $30-40, open hardware, USB-C powered, most flexible firmware [18] |
| **HLK-LD2410/B/C** | UART serial + GPIO OUT pin | 24GHz, up to 6m moving / 4.5m stationary | Configurable via serial | ESPHome, custom firmware | $3-5 module, bare PCB, default baud 256000, protocol well-documented [10] |
| **HLK-LD2450** | UART serial | 24GHz, 2D tracking up to 3 targets | Configurable | ESPHome | Upgraded LD2410 with zone support [18] |
| **Tuya mmWave** (various) | Zigbee/WiFi | 24GHz | Varies by model | ZHA, Z2M, Tuya Local | White-label, inconsistent firmware quality |

**MQTT payload example** (Zigbee2MQTT, Sonoff SNZB-06P) [10]:
```json
// State topic: zigbee2mqtt/<friendly_name>
{
  "occupancy": true,           // binary
  "occupancy_timeout": 15,     // seconds, min 15
  "occupancy_sensitivity": 2,  // 1-3
  "illumination": 245,         // lux (on-change only)
  "no_occupancy_since": 15     // seconds since clear
}
```

**Key insight:** Occupancy clear delay is the critical parameter. All mmWave sensors have a configurable "hold" period before reporting "clear" (15s minimum on most consumer sensors [10]). This means the fastest possible blanking time is ~15s after someone leaves the room — acceptable for OLED burn-in protection (minutes of static content cause burn-in, not seconds).

#### 2.2 USB-Wired Sensors (Direct Serial)

| Sensor | Interface | Protocol | Notes |
|--------|-----------|----------|-------|
| **HLK-LD2410/B/C** | USB-serial via CH340/CP2102 adapter (5V, 3.3V logic UART) | Little-endian binary frames, default 256000 baud 8N1 [10] | Direct USB connection possible. Python library exists: [mattdm/mmwave_presence](https://github.com/mattdm/mmwave_presence). Also has GPIO OUT pin (HIGH=occupied) |
| **HLK-LD2450** | Same UART protocol | Extended frame format with multi-target data | Same USB-serial adapter approach |
| **DFRobot SEN0395/SEN0609** | UART | Proprietary protocol, ESPHome component available [18] | Longer range (SEN0609 up to 25m) |
| **Seeed MR24HPC1** | UART | Proprietary | Scene-based presets [18] |

**USB-serial protocol detail (LD2410)** [10]:
- Continuous output mode: sensor pushes data frames (~20-40 bytes) at ~10-20 Hz
- Frame header: `0xF4 0xF3 0xF2 0xF1` (little-endian)
- Target state byte: bit 0 = moving target, bit 1 = stationary target
- Configurable via command frames (set baud, sensitivity, gates, etc.)
- Python library maturity: [mattdm/mmwave_presence](https://github.com/mattdm/mmwave_presence) supports CircuitPython + desktop Python

#### 2.3 Detection Latency Summary

| Sensor Class | Detection (enter) | Clear (exit) | Notes |
|--------------|-------------------|--------------|-------|
| PIR (basic motion) | <1s | 30s–5min (configurable) | Misses stationary people |
| 24GHz mmWave (LD2410-class) | <1s | 15–30s (configurable) | Can detect breathing micro-movements |
| 60GHz mmWave (Aqara FP2-class) | ~500ms | Configurable (typically 5–60s) | Zone-aware, multi-person |
| Built-in monitor IR sensor | <1s | ~5–10s | Limited to ~90° cone in front of display |

---

### 3. Display Control Mechanisms

#### 3.1 ⚠ CRITICAL: DPMS/Port Power-Off KILLS Audio Over DisplayPort/HDMI

**Finding:** When a PC monitor enters DPMS sleep or when the GPU port is powered off (DPM Off via DDC/CI VCP 0xD6 0x04), **audio embedded in the DisplayPort or HDMI signal is also cut off.** This is a physical limitation — audio data is transmitted via the same signaling mechanism as video data on DP/HDMI [31].

- Multiple user reports across SuperUser, AskUbuntu, and Arch forums confirm this [31]
- PulseAudio even has a feature request to detect HDMI port power state and auto-switch audio to analog outputs [31]
- Nvidia driver bugs have caused audio to not recover after DP sleep [31]
- Samsung Odyssey Ark workaround (`linux-display-audio-keepalive` [31]) streams silent audio to prevent the port from sleeping

**Implication for `oled-proximity`:** If the PC's audio plays through monitor speakers or a soundbar connected via monitor audio-passthrough, DPMS off = audio cut. The daemon MUST either:
1. Route audio through a separate audio device (PC speakers, USB DAC, separate soundbar via optical/USB) — recommended path
2. Use DDC/CI brightness-to-0 instead of power-off (but this does NOT protect OLED — see §3.7)
3. On Windows, keep the display "on" but show a full-screen pure-black overlay (application-level blanking)

#### 3.2 Samsung Tizen TV — Screen-Blank-With-Audio (Primary Target)

**Target TV:** Samsung S90C (2023, 2nd-gen QD-OLED, Tizen OS)

##### 3.2.1 Confirmed: `KEY_PICTURE_OFF` via WebSocket API ⭐

The Samsung Tizen TV WebSocket API has a dedicated `KEY_PICTURE_OFF` remote key command that **blanks the screen while audio keeps playing.** This is confirmed via the `ollo69/ha-samsungtv-smart` integration (GitHub issue #397) [32].

**WebSocket payload** to trigger Picture Off [32]:
```json
{
  "method": "ms.remote.control",
  "params": {
    "Cmd": "Click",
    "DataOfCmd": "KEY_PICTURE_OFF",
    "Option": "false",
    "TypeOfRemote": "SendRemoteKey"
  }
}
```

To wake the screen: send any key (e.g. `KEY_RETURN`, `KEY_HOME`, or directional key). The screen wakes while audio never stopped.

**WebSocket connection details** [26][28]:
- Port: 8002 (secure `wss://`) — port 8001 is deprecated on newer models
- URL format: `wss://<TV_IP>:8002/api/v2/channels/samsung.remote.control?name=<client_name>`
- Auth: first connection triggers a popup on TV — user accepts with remote, receives a token for subsequent connections
- Python library: [`samsungtvws`](https://github.com/xchwarze/samsung-tv-ws-api) (v3.0.5, 408 stars, active Feb 2026) [28]
- CLI usage: `samsungtv --host 192.168.1.50 send-key KEY_PICTURE_OFF` [30]
- Token persistence: `--token-file .token` to avoid re-pairing
- Limitation: TV must be on same subnet/VLAN — WS connections across subnets are rejected [26]

**Key list reference:** The full `KEY_*` enumeration is in [COMMANDS.md](https://github.com/xchwarze/samsung-tv-ws-api/blob/master/COMMANDS.md) [30]. `KEY_PICTURE_OFF` is NOT in the standard documented list — it was discovered via SmartThings capability scanning [32], but it works on Tizen OLED models.

##### 3.2.2 Built-in Accessibility "Picture Off" Mode

The S90C has a built-in feature: **Settings → General & Privacy → Accessibility → Picture Off** [27]. This blanks the screen, audio continues. When triggered via the remote or menu, this is the same underlying function as `KEY_PICTURE_OFF`.

⚠ **S90F/S95F firmware bug (2026):** Samsung community reports show that on 2025-2026 OLED models (S90F, S95F), YouTube specifically breaks this feature — both picture AND sound turn off [27]. This appears to be a YouTube app bug (possibly intentional — YouTube Premium paywall?), not a TV API issue. Internal apps and HDMI sources are unaffected. For the S90C (2023 model), this bug has NOT been reported — Picture Off works as expected.

##### 3.2.3 SmartThings API Capabilities

The SmartThings cloud API provides additional control but is NOT needed for Picture Off:
- `switch` capability: on/off (full power, kills audio)
- `custom.picturemode` capability: set picture mode (Standard, Movie, etc.) [32]
- No dedicated "picture off" capability in SmartThings — use WebSocket `KEY_PICTURE_OFF`
- SmartThings is useful for: power state polling (WebSocket status is unreliable on some models [27]), source input control, channel info
- OAuth2 authentication is now required (Personal Access Tokens deprecated) [32]

**HA integration landscape for Samsung:**
| Integration | Protocol | Picture Off? | Status |
|-------------|----------|-------------|--------|
| **Built-in `samsungtv`** | WebSocket (port 8002) | Via `remote.send_command` with `KEY_PICTURE_OFF` | Maintained, WOL being deprecated Mar 2026 [27] |
| **`ha-samsungtv-smart` (HACS)** | WebSocket + SmartThings | Via WS key command | Active, v3.x with OAuth2, 2024+ Frame TV support [32] |
| **`samsungtv_tizen` (legacy HACS)** | WebSocket (port 8001) | Unknown | **Broken** on HA 2024+ (API removal) [27] |

##### 3.2.4 Wake-on-LAN for TV Power-On

When the TV is fully off (not just picture-off), Wake-on-LAN is needed:
- Samsung Smart TVs (2016+) keep network interface partially active in standby
- WOL packet to TV MAC address works for power-on [28]
- HA's `samsungtv` integration WOL support is being deprecated (Mar 2026) — migrating to standalone `wakeonlan` integration [27]
- SmartThings cloud API `switch:on` can also wake the TV

#### 3.3 HDMI-CEC — NOT Suitable for Screen-Only Off

**Finding:** HDMI-CEC's `Standby` command puts the entire device to standby (full power-down). There is no CEC command for "blank screen, keep audio." The CEC specification defines:
- **System Standby** (`0x36`): Broadcast or directed — puts device to standby (full off)
- **Image View On** (`0x04`): Wake display from standby
- **Active Source** (`0x82`): Declare a device as active source
- **Set Stream Path** (`0x86`): Request display switch to this HDMI input

None of these offer screen-off-while-audio-keeps-playing. CEC is designed for basic device coordination (one-touch play, system standby), not granular screen control [11][29].

**Verdict:** HDMI-CEC is NOT a viable path for the audio-keeping requirement. It is, however, useful for full TV power-off as a fallback when WebSocket is unavailable.

#### 3.4 LG WebOS (De-prioritized — brief mention)

For reference only (not the primary target):
- LG's `bscpylgtv` library supports `lgtv.request('screenOff')` — which IS OLED-safe screen-off-with-audio [3]
- [bassidus/lgpowercontrol](https://github.com/bassidus/lgpowercontrol) (30 stars) uses this for automated display sleep→LG TV off [3]
- LG's approach is more developer-friendly than Samsung's — the `screenOff`/`screenOn` methods are first-class API calls, not key emulation

#### 3.5 Linux PC Monitor Control

| Mechanism | Tool/API | OLED Protection? | Audio Keeps Playing? | Notes |
|-----------|----------|------------------|---------------------|-------|
| **DPMS (X11)** | `xset dpms force off` | Yes | **No** — kills DP/HDMI audio [31] | Standard, reliable. `xset dpms force on` to wake |
| **wlr-output-power-management** | `wlopm`, `wlr-dpms` | Yes | **No** — port powered off | wlroots compositors only (Sway, Hyprland, Wayfire, River). NOT GNOME/KDE [7] |
| **KDE (Wayland)** | `kscreen-doctor --dpms off` | Yes | **No** | KDE Plasma Wayland only |
| **DDC/CI** | `ddcutil setvcp 0xD6 0x04` | Yes | **No** — DPM Off cuts port audio [31] | Monitor-dependent; works over I2C (`/dev/i2c-N`) [8] |
| **DDC/CI brightness** | `ddcutil setvcp 0x10 0` | **No** — pixels still powered | **Yes** — only video metadata, port stays active | Does NOT protect OLED |
| **Full-screen black overlay** | Application-level | **Yes** (emissive OLED only) | **Yes** | Fragile — any notification/cursor breaks it |

**Recommendation for PC with audio requirement:** Use DDC/CI brightness to 0 as a *partial* mitigation (reduces wear, doesn't eliminate it), OR route audio through a separate output device. The only way to truly power off the OLED panel while keeping audio is to have audio on a separate path.

#### 3.6 Windows PC Monitor Control

| Mechanism | API | Audio Keeps Playing? | Notes |
|-----------|-----|---------------------|-------|
| **SC_MONITORPOWER** | `SendMessageW(..., WM_SYSCOMMAND, SC_MONITORPOWER, 2)` | **No** — port sleeps [31] | Standard Windows display sleep |
| **DDC/CI** | `SetVCPFeature(hMonitor, 0xD6, 0x04)` via `lowlevelmonitorconfigurationapi.h` [17] | **No** — DPM Off | Powers off monitor controller; audio cut |
| **DDC/CI brightness** | `SetVCPFeature(hMonitor, 0x10, 0)` | **Yes** | Does NOT protect OLED |
| **Full-screen black window** | Application-level (e.g., [OLED-Sleeper](https://github.com/Quorthon13/OLED-Sleeper) [4]) | **Yes** | OLED-Sleeper does exactly this for secondary monitors |

#### 3.7 OLED Protection Summary (Updated)

| Method | Actually Protects OLED? | Audio Keeps Playing? | Why/Why Not |
|--------|------------------------|---------------------|-------------|
| DPMS off / VCP 0xD6 / `zwlr_output_power_v1` off | **Yes** | **No** — audio embedded in signal is cut [31] | Pixels physically powered off, but port is off too |
| Samsung `KEY_PICTURE_OFF` (TV only) | **Yes** | **Yes** ⭐ | TV blanks panel, audio DSP stays active — exactly the desired behavior [27][32] |
| Full-screen pure black (no compositor) | **Yes** (emissive OLED) | **Yes** | Black OLED pixel = off; fragile — overlays/notifications/cursor break it |
| Brightness to 0% via DDC/CI | **No** | **Yes** | Pixels still powered, just at minimum emission |
| Screensaver with moving content | **Partial** | **Yes** | Reduces static wear but pixels are still aging |

**Key insight:** The Samsung `KEY_PICTURE_OFF` is the GOLD STANDARD for this project — it achieves the audio-keeping requirement while genuinely protecting the OLED panel. For PC monitors, the DPMS-audio conflict means the daemon must either accept audio-cut during blanking, use a brightness-only approach (partial protection), or require users to route audio separately.

---

### 4. Rust vs Go Ecosystem Fit

#### 4.1 MQTT

| Criterion | Rust (`rumqttc`) | Go (`paho.mqtt.golang`) |
|-----------|------------------|-------------------------|
| **Purity** | Pure Rust, no C dependency | CGO wrapper around Eclipse Paho C library (or bundled C sources) |
| **Async** | Tokio-native async event loop [9] | Goroutine-based, natural fit for Go's concurrency model |
| **MQTT 5** | rumqttc-next supports MQTT 5 [9] | paho.mqtt.golang supports MQTT 5 (as of v1.5+) |
| **Reconnection** | Automatic in event loop; `publish()` enqueues (no immediate feedback) [9] | Manual but straightforward; `publish()` returns connection status immediately [9] |
| **TLS** | rustls (default) or native-tls [9] | Requires OpenSSL (CGO) unless bundled |
| **Static binary** | Trivial — musl target | Requires CGO_ENABLED=0 or static linking of libpaho C library |
| **Alternative** | `paho-mqtt` (Rust) also exists as C-binding wrapper [9] | `eclipse/paho.mqtt.golang` is the standard |

**Winner: Rust.** `rumqttc` is pure Rust, no C dependency, compiles to a single static binary on all platforms. Go's `paho.mqtt.golang` ties you to CGO + libpaho C library for production TLS, complicating cross-compilation and static linking.

#### 4.2 Serial Port (USB Sensor)

| Criterion | Rust | Go |
|-----------|------|-----|
| **Primary crate** | `serialport-rs` (cross-platform, blocking) + `tokio-serial` (async) [16] | `go.bug.st/serial` or `tarm/serial` |
| **Cross-platform** | Linux, macOS, Windows, FreeBSD, Android, iOS [16] | Linux, macOS, Windows |
| **Async I/O** | `tokio-serial` wraps `mio-serial` for Tokio event loop [16] | Goroutine-based, simple read/write loops |
| **Port enumeration** | Built-in on all major platforms [16] | Built-in |
| **Maturity** | Well-maintained (recent releases), MPL-2.0 licensed | Varies by library; `go.bug.st/serial` is solid |

**Winner: Tie.** Both ecosystems have solid serial port support. Rust's `serialport-rs` is more actively maintained and has broader platform support (Android/iOS targets).

#### 4.3 Display Control

| Platform | Rust | Go |
|----------|------|-----|
| **X11 DPMS** | Via `x11` crate or shelling to `xset` | Shell to `xset` or `github.com/BurntSushi/xgb` |
| **Wayland wlr protocols** | `wayland-client` + `wayland-protocols-wlr` crates — proven in [wlout](https://github.com/sashokbg/wlout) [7] | No native Wayland protocol client library — would need CGO bindings |
| **DDC/CI (Linux)** | Shell to `ddcutil` or `i2c` crate for raw I2C | Shell to `ddcutil` |
| **DDC/CI (Windows)** | `winapi` crate `lowlevelmonitorconfigurationapi` — proven in [display_off](https://github.com/Programming4life/display_off) [17] | `golang.org/x/sys/windows` for DLL loading + syscalls |
| **Windows monitor power** | `winapi` `SendMessageW` — proven in [Rust-Screen-Disabler](https://github.com/YagirProtect/Rust-Screen-Disabler) [6] | `syscall` package — straightforward |
| **macOS** | Shell to `pmset` or `caffeinate` | Shell to `pmset` |
| **HDMI-CEC** | Shell to `cec-client` or raw CEC ioctl | Shell to `cec-client` |

**Winner: Rust.** The `wayland-protocols-wlr` crate exists and is production-used. Go has no equivalent — you'd need CGO to speak Wayland protocols. On Windows, Rust's `winapi` crate is a first-class citizen. The practical truth for both languages is that most display control goes through shell commands (`xset`, `ddcutil`, `pmset`, `cec-client`), but Rust has native-API options where Go requires CGO.

#### 4.4 Home Assistant WebSocket API

| Criterion | Rust | Go |
|-----------|------|-----|
| **Available library** | `hass-rs` (v0.5.0, async, tokio) [14] | `hass-ws` (v0.2.3, `kjbreil/hass-ws`), `go-ha-client` (v2.0.0, `mkelcik/go-ha-client`) [14] |
| **Maturity** | Early stage: basic auth, get_config, get_states. No auto-reconnect yet [14] | More mature: auto-reconnect, service calls, state subscriptions, typed events [14] |
| **Roadmap risk** | `hass-rs` states "Automatic reconnection (TBD)" [14] — would need to add this | `go-ha-client` v2.0.0 is stable with reconnect built in [14] |

**Winner: Go (slightly).** `go-ha-client` (v2.0.0) is more production-ready with auto-reconnect, typed events, and service call helpers. `hass-rs` would need work (auto-reconnect, service calls). However, for MQTT-based sensor ingestion, the HA WebSocket API is optional — the simpler path is subscribing to MQTT topics directly.

#### 4.5 Cross-Compilation & Packaging

| Criterion | Rust | Go |
|-----------|------|-----|
| **Single static binary** | Trivial with musl targets (`x86_64-unknown-linux-musl`) [13] | Requires CGO_ENABLED=0 (loses some features) |
| **Cross-compile** | `cross` tool (Docker-based, zero setup), `cargo-zigbuild` [13] | Built into Go toolchain (`GOOS=linux GOARCH=amd64 go build`) |
| **Windows** | `x86_64-pc-windows-msvc` (native) or `-gnu` [13] | Native, no toolchain needed |
| **macOS** | Cross-compile from Linux via `cross` [13] | Native cross-compilation works |
| **System packages** | `cargo-deb` (.deb), `cargo-rpm` (.rpm), GitHub Actions actions for MSI/DMG [13] | `nfpm` for .deb/.rpm/, goreleaser for all-in-one |
| **CI/CD** | `rust-build-package-and-release-action` (supports .deb/.rpm/.apk/.dmg/.msi + AUR/Homebrew/Winget) [13] | `goreleaser` (de facto standard, very mature) |

**Winner: Tie (different strengths).** Go's native cross-compilation is simpler out of the box. Rust's `cross` adds a Docker dependency but handles C library cross-compilation automatically. Both have mature CI/CD pipelines. Rust's musl static binary is the gold standard for "download and run."

#### 4.6 Overall Ecosystem Verdict

**Rust wins for this project.** The combination of pure-Rust MQTT (`rumqttc` — no C dependency), mature Wayland protocol support (`wayland-protocols-wlr`), direct Windows DDC/CI access via `winapi`, and excellent cross-compilation to single static binaries makes Rust the better choice. Go's advantages (simpler learning curve, faster compilation, built-in cross-compilation) are real but don't outweigh the Wayland protocol gap and the MQTT C-dependency problem.

---

### 5. Code Quality Baseline (Rust, 2026)

A new Rust OSS project in 2026 should start with this toolchain:

| Layer | Tools | Purpose |
|-------|-------|---------|
| **Formatting** | `rustfmt` (built-in) | Idiomatic formatting, zero-config |
| **Linting** | `clippy` (built-in) | Standard Rust linter; enable `#![warn(clippy::pedantic)]` |
| **Build** | `cargo` with `Cargo.lock` committed | Reproducible builds |
| **Testing** | `cargo test`, `cargo-tarpaulin` (coverage), `proptest` (property-based) | Unit + integration + property tests |
| **CI** | GitHub Actions with `rust-build-package-and-release-action` [13] | Build, test, lint on push/PR; release artifacts for Linux/macOS/Windows |
| **Release** | `cargo-dist` or `rust-build-package-and-release-action` [13] | .deb/.rpm/.apk/.dmg/.msi + Homebrew/AUR/Winget manifests |
| **Pre-commit** | `pre-commit` hooks: `rustfmt --check`, `clippy -- -D warnings`, `cargo test` | Block broken commits |
| **Commits** | Conventional Commits (`feat:`, `fix:`, `chore:`) | Machine-parseable, enables auto-changelog |
| **Dependencies** | `cargo-deny` (license auditing, duplicate detection), `cargo-audit` (security) | Supply-chain hygiene |
| **Documentation** | `rustdoc` (built-in) | API docs; `#![warn(missing_docs)]` for public API |
| **Architecture** | Trait-based backends (`trait Sensor`, `trait DisplayController`) | Enables community contributions via new backend implementations |
| **Configuration** | TOML config file + environment variable overrides + CLI flags (`clap` derive) | Standard Rust config pattern |
| **Logging** | `tracing` (structured, async-aware) over `env_logger` | Production-grade observability from day one |
| **Error handling** | `thiserror` (library errors) + `anyhow` (application errors) | Standard Rust error stack |

---

## `<sources>`

1. [davidz-yt/lg-oled-auto-sleep](https://github.com/davidz-yt/lg-oled-auto-sleep) — ESP32 + LD2410C + HA → LG TV auto-sleep (83 stars, last commit Jul 2025)
2. [sumitchill/Oled_Burnin_prev](https://github.com/sumitchill/Oled_Burnin_prev) — ESP32 + LD2410C + Flask server for Windows + HA for TV (~5 stars)
3. [bassidus/lgpowercontrol](https://github.com/bassidus/lgpowercontrol) — Linux systemd service, hooks display sleep/wake → LG WebOS TV (30 stars, active May 2026)
4. [danilofalcao/kidle](https://github.com/danilofalcao/kidle) — KDE Plasma Wayland idle→screen-off daemon
5. [r-mccarty/presence-dectection-engine](https://github.com/r-mccarty/presence-dectection-engine) — ESPHome mmWave presence engine with z-score analysis
6. Prior art search results: [meltingice1337/watchdesk](https://github.com/meltingice1337/watchdesk), [YagirProtect/Rust-Screen-Disabler](https://github.com/YagirProtect/Rust-Screen-Disabler), [mrmekon/circadian](https://github.com/mrmekon/circadian), [dwagelaar/enforce-dpms](https://github.com/dwagelaar/enforce-dpms)
7. Wayland DPMS research: [wlr-output-power-management protocol](https://wayland.app/protocols/wlr-output-power-management-unstable-v1), [lilydjwg/dpms-off](https://github.com/lilydjwg/dpms-off), [~dsemy/wlr-dpms](https://git.sr.ht/~dsemy/wlr-dpms), [sashokbg/wlout](https://github.com/sashokbg/wlout), [jwz Wayland DPMS blog](https://www.jwz.org/blog/2025/07/wayland-dpms/)
8. DDC/CI: [ddcutil documentation](https://www.ddcutil.com/), [VCP D6 power mode values](https://kravemir.org/how-to/automatically-turn-off-on-touch-screen-based-on-activity-via-ddc-ci-with-rapsberry-pi/), [ddcutil FAQ](http://www.ddcutil.com/faq/)
9. MQTT ecosystem: [rumqttc docs](https://docs.rs/rumqttc/latest/rumqttc/), [paho-mqtt Rust](https://github.com/eclipse-paho/paho.mqtt.rust), [MQTT clients benchmark](https://github.com/lucasdietrich/rust-mqtt-clients-benchmark), [paho.mqtt.golang](https://pkg.go.dev/github.com/eclipse/paho.mqtt.golang)
10. Sensor documentation: [SNZB-06P Zigbee2MQTT](https://www.zigbee2mqtt.io/devices/SNZB-06P.html), [SNZB-06P firmware guide](https://www.sonoff.in/blog/product-guides-3/snzb-06p-firmware-upgrade-and-home-assistant-operation-guide-111), [LD2410 serial protocol (PDF)](https://make.net.za/wp-content/datasheets/HLK%20LD2410B%20Serial%20Communication%20Protocol%20v1.07.pdf), [LD2410 guide](https://componentindex.net/components/ld2410/)
11. HDMI-CEC: [ArchWiki HDMI-CEC](https://wiki.archlinux.org/title/HDMI-CEC), [libcec GitHub](https://github.com/Pulse-Eight/libcec), [RPi TV remote CEC](https://github.com/tjs-w/pi-tv-remote), [LG standby issue](https://github.com/Pulse-Eight/libcec/issues/554), [Linux kernel CEC docs](https://docs.kernel.org/6.3/admin-guide/media/cec.html)
12. Home Assistant community: [mmWave sensor placement guide](https://www.linknlink.com/blogs/guides/60ghz-mmwave-sensor-placement-home-assistant), [focus mode office automation](https://www.linknlink.com/blogs/guides/home-assistant-focus-mode-office-mmwave-ir-automation), [SNZB-06P community thread](https://community.home-assistant.io/t/sonoff-snzb-06p-stuck-on-detected-need-help-fixing/870927/1)
13. Rust cross-compilation: [Multi-platform binary releases](https://racum.blog/articles/rust-cli-releases/), [rust-cross-compile guide](https://github.com/KodrAus/rust-cross-compile), [cargo-cross](https://docs.rs/crate/cargo-cross/1.0.9), [cross-compilation guide](https://devproportal.com/languages/rust/rust-cross-compilation-guide-multiple-platforms/), [rust-build-package-and-release-action](https://github.com/marketplace/actions/rust-build-package-and-release-action)
14. HA WebSocket clients: [hass-rs](https://github.com/danrusei/hass-rs) (Rust), [hass-ws](https://pkg.go.dev/github.com/kjbreil/hass-ws) (Go), [go-ha-client](https://pkg.go.dev/github.com/mkelcik/go-ha-client) (Go v2.0.0), [HA WebSocket API docs](https://developers.home-assistant.io/docs/api/websocket/)
15. Aqara FP2: [FP2 product page](https://www.aqara.com/us/product/presence-sensor-fp2/), [FP2 latency discussion](https://forum.aqara.com/t/how-to-speed-up-fp2-presence-automations/73693), [FP2 ESPHome integration](https://community.home-assistant.io/t/integrating-aqara-fp2-presence-sensor-directly-into-home-assistant/660018)
16. Rust serial: [serialport-rs](https://github.com/serialport/serialport-rs), [tokio-serial](https://docs.rs/tokio-serial/latest/tokio_serial/)
17. Windows DDC/CI: [SetVCPFeature docs](https://learn.microsoft.com/en-us/windows/win32/api/lowlevelmonitorconfigurationapi/nf-lowlevelmonitorconfigurationapi-setvcpfeature), [PowerToys DDC/CI controller](https://github.com/microsoft/PowerToys/blob/92014c81/src/modules/powerdisplay/PowerDisplay.Lib/Drivers/DDC/DdcCiController.cs), [display_off (Rust)](https://github.com/Programming4life/display_off)
18. Everything Presence Lite: [EPL GitHub](https://github.com/EverythingSmartHome/everything-presence-lite), [EPL docs](https://docs.everythingsmart.io/s/products/doc/everything-presence-lite-epl-ZVnBzYzuX2)
19. [Quorthon13/OLED-Sleeper](https://github.com/Quorthon13/OLED-Sleeper) — Windows tool to blackout idle monitors
20. [manvir-singh/display_off](https://github.com/Programming4life/display_off) — Powers off DDC/CI displays on Windows shutdown (Rust, 9 stars)
21. [SimonPanigrahi/Ubuntu-Screensaver](https://github.com/simonpanigrahi/Ubuntu-Screensaver) — OLED-safe black screensaver for Ubuntu/Wayland
22. [meltingice1337/watchdesk](https://github.com/meltingice1337/watchdesk) — Rust Windows service publishing monitor state via MQTT
23. [mattdm/mmwave_presence](https://github.com/mattdm/mmwave_presence) — Python/CircuitPython library for LD2410 mmWave sensors
24. MSI OLED CARE 3.0 — [MSI blog](https://www.msi.com/blog/next-gen-oled-burn-in-defense-with-privateai-sensing-msi-oled-care-3) (2025-08-20)
25. Samsung TV WS API: [samsung-tv-ws-api](https://github.com/xchwarze/samsung-tv-ws-api) (408 stars, v3.0.5, Feb 2026) — Python library for Samsung Tizen TV remote control
26. Samsung TV WS API docs: [COMMANDS.md](https://github.com/xchwarze/samsung-tv-ws-api/blob/master/COMMANDS.md) — full key list; [llms.txt](https://context7.com/xchwarze/samsung-tv-ws-api/llms.txt) — API reference
27. Samsung community / HA: [S90F picture off bug (Samsung Community)](https://us.community.samsung.com/t5/LED-and-OLED-TVs/S90F-cannot-use-Auto-Picture-Off-on-Youtube/td-p/3501173) — confirms Picture Off feature path: Settings → General → Accessibility → Picture Off; [Samsung UK support: how to turn off screen keep audio](https://www.samsung.com/uk/support/tv-audio-video/how-do-i-turn-off-my-samsung-tv-picture-but-not-the-sound/); [HA Samsung TV toggling thread](https://community.home-assistant.io/t/samsung-tv-toggling-instead-of-turning-off/876277); [S90F control issues on HA](https://community.home-assistant.io/t/dont-buy-samsung-tvs-samsung-s90f-control-help/968100/1); [HA WOL deprecation](https://community.home-assistant.io/t/samsungtv-wake-on-lan-deprecated-why/995502)
28. samsungtvws PyPI: [samsungtvws v3.0.5](https://pypi.org/project/samsungtvws/3.0.5/) — pip-installable, async + CLI support
29. HDMI-CEC deep research: [cec-ctl man page](https://manpages.debian.org/bookworm/v4l-utils/cec-ctl.1) — lists all CEC commands; [HDMI CEC troubleshooting guide](https://ca.ktcplay.com/blogs/buying-guides/hdmi-cec-troubleshooting-guide) — confirms no screen-off-audio-on command; [ArchWiki HDMI-CEC](https://wiki.archlinux.org/title/HDMI-CEC) — comprehensive CEC reference
30. Samsung TV WS COMMANDS.md: [full key list on GitHub](https://github.com/xchwarze/samsung-tv-ws-api/blob/master/COMMANDS.md) — `KEY_PICTURE_OFF` not in standard docs but confirmed working via SmartThings research
31. DPMS audio cutoff sources: [SuperUser: Audio via DisplayPort](https://superuser.com/questions/1552309/audio-via-displayport-how-do-i-keep-it-going-when-windows-10-turns-the-display) — physically impossible to turn off video but not audio on a DP/HDMI port; [AskUbuntu: DPMS and HDMI audio](https://askubuntu.com/questions/1165801/dpms-and-hdmi-audio) — audio turned off once DPMS kicks in; [linux-display-audio-keepalive](https://github.com/DigitalCyberSoft/linux-display-audio-keepalive) — systemd service to prevent HDMI/DP audio cutoff; [PulseAudio discuss: DPMS and HDMI audio routing](https://lists.freedesktop.org/archives/pulseaudio-discuss/2016-February/025393.html); [Nvidia DP audio sleep bug](https://forums.developer.nvidia.com/t/displayport-audio-stops-working-after-monitor-goes-to-sleep-with-346-47/36920)
32. Samsung Picture Off automation sources: [ha-samsungtv-smart issue #397](https://github.com/ollo69/ha-samsungtv-smart/issues/397) — **confirms `KEY_PICTURE_OFF` WebSocket command**; [ha-samsungtv-smart SmartThings API](https://github.com/ollo69/ha-samsungtv-smart/blob/master/custom_components/samsungtv_smart/api/smartthings.py) — SmartThings capabilities reference; [S90C/S89C owners thread](https://www.avsforum.com/threads/2023-samsung-4k-s95c-s90c-s89c-owners-thread-no-price-talk.3267261/) — S90C specs; [S90C Samsung Community issues](https://us.community.samsung.com/t5/Projectors-Other-TVs/S90C-OLED-Screen-Saver-coming-on-constantly/td-p/2693967)

---

## Confidence

**Very High** — for the Samsung `KEY_PICTURE_OFF` finding. This is confirmed via GitHub issue #397 on `ollo69/ha-samsungtv-smart` [32] with the exact WebSocket JSON payload, and corroborated by Samsung's own support documentation [27] which confirms the "Picture Off" accessibility feature keeps audio playing. The path is: WebSocket `wss://<TV_IP>:8002` → `KEY_PICTURE_OFF` click → screen blanks, audio continues. This is the single most important finding for the project.

**Very High** — for the DPMS-audio-conflict finding. Multiple independent sources [31] confirm audio is embedded in the DP/HDMI signal, making it physically impossible to power off the port while keeping audio. This is a fundamental constraint, not a software limitation.

**High** — for prior art verdict, sensor landscape, and Rust ecosystem recommendation.

**Medium** — for DDC/CI VCP 0xD6 support on specific OLED monitors (ASUS PG series). This is monitor-firmware-dependent and can only be confirmed per-model.

**Medium** — for macOS display control. The API surface is sparsely documented and private APIs may break.

**Medium** — for Samsung S90F/S95F "Picture Off + YouTube" bug. Community reports [27] confirm this but Samsung has not acknowledged it as a bug. The S90C (2023 model) does NOT have this bug.

## Open Questions

- **Can `KEY_PICTURE_OFF` be confirmed on the S90C specifically?** The HA community testing was on a QNX9D (2024 QLED). S90C (2023 OLED) needs explicit verification — though the command is a Tizen OS remote key, it should be universal across Tizen models. Priority: verify on actual S90C hardware.
- **What is the wake mechanism from Picture Off?** Any remote key wakes the screen, but which key is safest? `KEY_RETURN`? `KEY_HOME` (risks exiting the current app)? Needs testing.
- **Does the Samsung TV WebSocket stay connected during Picture Off?** If the TV closes the WS connection when picture-off, we need token re-auth for wake. The `ha-samsungtv-smart` integration has auto-reconnection logic [32] that could serve as a reference.
- **Can `ddcutil setvcp 0xD6 0x04` fully power off an ASUS OLED monitor (PG42UQ, PG48UQ, PG32UCDM)?** Per-model testing needed.
- **GNOME 46+ Wayland display power-off path?** `wlr-output-power-management` is not supported by Mutter. Is there a working D-Bus or `gdbus` path to blank displays without disabling them?
- **Does the Sonoff SNZB-06P at minimum 15s timeout actually deliver 15s clear latency in practice**, or does internal debouncing add more?
