# Introduction

dormant is a Rust daemon that blanks OLED PC monitors and TVs when presence sensors report an empty room. It wakes displays instantly on return. Sensors come in via MQTT, Home Assistant WebSocket, or USB-serial mmWave radar. Displays are controlled with DDC/CI, KWin DPMS (Linux), Samsung Tizen, Home Assistant passthrough, or arbitrary shell commands.

## Why dormant?

OLED panels burn in. Static UI elements — taskbars, window decorations, desktop icons — degrade the panel over hundreds of hours. Dimming the display or turning it off when no one is watching extends panel life.

Built-in monitor proximity sensors exist (MSI OLED CARE, Eve/Dough Neo), but they only protect one monitor and cannot coordinate with room-level presence. dormant fuses multiple sensors across multiple zones and controls every display in the room, including TVs.

## How it works at a high level

1. **Sensors** report presence — a Zigbee mmWave radar over MQTT, a Home Assistant binary sensor, or a USB-LD2410 module.
2. **Zones** fuse sensors with `any`, `all`, `quorum`, or `weighted` logic. A zone is "occupied" or "vacant".
3. **Rules** link zones to displays with timing parameters: grace periods prevent rapid toggling, min-blank/min-wake floors prevent thrash, inhibitors (user activity, manual pause) override blanking.
4. **Displays** receive blank/wake commands through an ordered controller chain with fallback and retry.

## What dormant protects against

OLED panels degrade with static content. The effectiveness of each blank mode varies:

| Mode | OLED protection | Audio | Wake speed |
|---|---|---|---|
| `screen_off_audio_on` | Full (panel off, electronics on) | Yes | Fast (~1s) |
| `power_off` | Full (DPMS off, DDC power off) | No | Slower (monitor-dependent) |
| `brightness_zero` | Partial (pixels still powered, but minimal emission) | Yes | Instant |

Use `screen_off_audio_on` for TVs where audio should continue. Use `power_off` for PC monitors. `brightness_zero` is a fallback for controllers that support neither — it is better than nothing, but the panel is still energized.

## Fail-safe design

**Unavailable means present, never absent.** If a sensor stops reporting (broker down, USB unplugged, network loss), the zone treats it as *present*. dormant never blanks a display when it cannot confirm the room is empty. This is the single most important safety invariant in the codebase.
