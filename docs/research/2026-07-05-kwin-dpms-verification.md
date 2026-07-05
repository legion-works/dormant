# KWin DPMS — Hardware Spike Verification

> Spike conducted 2026-07-05 on KDE Plasma 6.7.2, Wayland session, RTX 5090
> with nvidia proprietary driver. Outputs: DP-1 (AOC AGON AG326UZD) and
> HDMI-A-1 (Samsung S90D TV, also wired as a PC output).

## Summary

Per-output DPMS via `kscreen-doctor --dpms off <output>` works cleanly on this
Plasma version. However, DPMS on any output that carries audio tears down the
output's ALSA sink, killing audio. This is architectural — DPMS disables the
output, and the associated sound device goes with it. There is no kernel or
compositor knob to prevent it.

**Verdict:** per-output DPMS GO but AUDIO-UNSAFE. kwin-dpms is a fallback
controller for outputs with no DDC/CI and no audio. For audio-carrying displays,
use `ddcci` (VCP 0xD6, which blanks the panel without touching the output) or
`samsung-tizen` (KEY_PICTURE_OFF, which is audio-safe by design).

## Per-output DPMS works

```bash
# All outputs
kscreen-doctor --dpms off      # works
kscreen-doctor --dpms on       # works

# Single output
kscreen-doctor --dpms off DP-1 # works
kscreen-doctor --dpms on DP-1  # works
kscreen-doctor --dpms off HDMI-A-1  # blanks TV, kills HDMI audio
```

## DPMS kills audio (verified)

Any DPMS command that touches HDMI-A-1 (or an all-output DPMS) disables the
output, which destroys the HDMI audio device in ALSA. The TV is the audio sink
for this machine — when DPMS hits it, audio stops immediately. This is not a
config setting; it is how the DRM/KMS pipeline works.

## Idle detection dead-end: GetSessionIdleTime

`qdbus` is not installed on this system. Calling
`org.freedesktop.ScreenSaver.GetSessionIdleTime` via `busctl` returns:

```
GetSessionIdleTime is not supported on this platform
```

This is an X11-era stub that is inert on Wayland. Do not use it.

The `logind` properties `IdleHint` and `IdleSinceHint` are boolean/timestamp
granularity — not suitable for fine idle-ms polling.

## Wayland-native idle protocol available

`wayland-info` advertises these relevant protocols:

| Protocol | Available | Notes |
|---|---|---|
| `org_kde_kwin_idle` v1 | Yes | KWin-specific |
| `zwp_idle_inhibit_manager_v1` v1 | Yes | Inhibit-only |
| `ext_idle_notifier_v1` v2 | Yes | Cross-compositor standard |

`ext_idle_notifier_v1` v2 is the correct Wayland-native idle source for M1
inhibitor polling (being folded into the implementation by a parallel task).

## Display-specific blanking strategy

| Display | Output | Audio sink? | DDC/CI? | Recommended controller |
|---|---|---|---|---|
| AOC AGON AG326UZD | DP-1 | No | Yes (VCP D6: 01/04/05) | `ddcci` power_off (audio-safe via VCP D6) |
| Samsung S90D TV | HDMI-A-1 | Yes | No ("Invalid display") | `samsung-tizen` picture-off (audio-safe by design) |

For any output with no DDC/CI and no audio, kwin-dpms is a valid fallback.
