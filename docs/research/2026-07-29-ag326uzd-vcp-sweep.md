# AOC AG326UZD — Read-only VCP Sweep

> Sweep conducted 2026-07-29 with the daemon stopped, on DDC/CI bus 4.
> Model: AG326UZD. MCCS version: 2.2.

## Method

Ran a read-only `getvcp` sweep against the panel while dormant was stopped. The
standard OLED-care-relevant codes were queried individually, followed by every
code from `0xE0` through `0xFF`. No VCP writes were attempted.

## Standard codes

| Code | Decoded meaning | Raw result | Dormant relevance |
|---|---|---|---|
| `0x02` | New control value | `SNC x03` | Generic monitor feature; no OLED-care meaning established |
| `0xB6` | Display technology type | `SNC x03` | Could support an automatic `panel_type` suggestion; do not replace EDID identification |
| `0xC0` | Display usage time | `C 1213 0` | Already consumed for aggregate-hours wear seeding; not a refresh counter |
| `0xC6` | Application enable key | `CNC x00 xff x00 x40` | No wear-state meaning established |
| `0xC8` | Display controller type | `CNC x00 x00 x00 x09` | No wear-state meaning established |
| `0xC9` | Manufacturer-specific / undocumented | `CNC xff xff x00 x01` | Preserve as raw evidence only |
| `0xCA` | OSD/button control | `CNC x00 x02 x00 x01` | No wear-state meaning established |
| `0xCC` | OSD language | `SNC x02` | English; not telemetry |
| `0xD6` | Power mode | `SNC x01` — DPM on, DPMS off | State/control input, not OLED telemetry |
| `0xDF` | MCCS/VCP version | `CNC xff xff x02 x02` | Confirms MCCS 2.2; use before interpreting standard codes |

`0xB6` is reported as value `0x03` by this panel. The standard catalog's OLED
value is `0x06`; therefore this result does not identify the AG326UZD as OLED
through `0xB6` alone, but the code remains useful for a future suggestion path.

## Manufacturer range disposition

The full `0xE0`–`0xFF` range was queried. Values below are verbatim `getvcp`
responses; `Unsupported` means the request returned `ERR`.

| Code | Disposition |
|---|---|
| `0xE0` | Unsupported |
| `0xE1` | Unsupported |
| `0xE2` | Answered — `CNC xff xff x02 x02` |
| `0xE3` | Unsupported |
| `0xE4` | Unsupported |
| `0xE5` | Unsupported |
| `0xE6` | Answered — `CNC x00 x00 x00 x00` |
| `0xE7` | Unsupported |
| `0xE8` | Unsupported |
| `0xE9` | Unsupported |
| `0xEA` | Unsupported |
| `0xEB` | Unsupported |
| `0xEC` | Unsupported |
| `0xED` | Answered — `CNC x00 x01 x00 x00` |
| `0xEE` | Unsupported |
| `0xEF` | Unsupported |
| `0xF0` | Unsupported |
| `0xF1` | Unsupported |
| `0xF2` | Unsupported |
| `0xF3` | Unsupported |
| `0xF4` | Unsupported |
| `0xF5` | Unsupported |
| `0xF6` | Unsupported |
| `0xF7` | Unsupported |
| `0xF8` | Answered — `CNC x00 x01 x00 x00` |
| `0xF9` | Unsupported |
| `0xFA` | Unsupported |
| `0xFB` | Unsupported |
| `0xFC` | Unsupported |
| `0xFD` | Unsupported |
| `0xFE` | Unsupported |
| `0xFF` | Unsupported |

The answered manufacturer-specific values have no decoded meaning established
by this sweep. They must remain opaque raw tuples keyed to the panel identity;
no writes or inferred OLED-care semantics are justified.

## Conclusion

AOC does **not** mirror the OSD pixel-refresh counters documented on the related
AG326UD (elapsed time and refresh count) into DDC/CI on this AG326UZD. Panel-
internal wear state is therefore unreachable from the host through the tested
VCP surface. Host-side estimation stands; `0xC0` remains useful only as an
aggregate usage-hours seed.
