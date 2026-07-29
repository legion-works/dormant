# OLED panel telemetry from a host

Research date: 2026-07-29. Scope: public documentation, shipped protocol/tool
source, public monitor material, and the AG326UZD read-only sweep.

**Verification status:** Independently verified 2026-07-29: 6/6 load-bearing
claims re-fetched; adversarial counter-search empty. The on-hardware
confirmation is documented in [the AG326UZD VCP sweep](2026-07-29-ag326uzd-vcp-sweep.md).

## Findings

### 1. OLED-specific DDC/CI surface

**Verdict: no portable OLED telemetry VCP surface found.** MCCS standard codes
identify/control a monitor, but the public sources examined do not document a
standard VCP for panel temperature, compensation-cycle count, logo/TPC/ABL
state, aggregate degradation, or a wear map. Manufacturer-specific `0xE0..0xFF`
values have no portable semantics. [1]

ddcutil warns that capability strings are informational and often wrong;
`getvcp`/`setvcp` testing establishes actual support. The AG326UZD sweep used
read-only `getvcp` probes on bus 4 with the daemon stopped. `0xB6`, `0xC0`,
`0xD6`, and `0xDF` answered as standard features; only `0xE2`, `0xE6`, `0xED`,
and `0xF8` answered in the manufacturer range, with opaque raw values.

The related AOC AG326UD manual documents OSD fields *Time after Pixel Refresh*
and *Pixel Refresh Counts*, plus pixel orbiting, logo protection, taskbar/
boundary dimming, and thermal protection. It documents DDC/CI as an enable
setting, not as a mapping for those fields. [2] The model mismatch made that a
lead; the AG326UZD sweep now confirms that those counters are not mirrored into
its tested DDC/CI surface.

### 2. Panel-internal wear and compensation data

**Verdict: no documented host path to a consumer OLED wear map was found.** The
useful care state is behind the monitor OSD. The manual demonstrates retained
elapsed-since-refresh and count state, but cannot establish that the fields
exist on every related model or are externally readable. [2]

DisplayCAL/ArgyllCMS can measure emitted-light uniformity with a colorimeter;
it does not read retained wear state or uniquely distinguish burn-in from other
nonuniformity. [6]

### 3. Samsung Tizen and TV diagnostics

**Verdict: Samsung MDC `Maintenance Control` does not transfer to the S90D
consumer API without target verification.** The consumer IP-control worksheet
contains no panel-care endpoint. The separate commercial MDC protocol defines
maintenance, panel-on-time, and temperature commands, but is not evidence for
the S90D's Tizen WebSocket or port-1516 JSON-RPC. [3][4]

### 4. Host-side per-region luminance estimation

**Verdict: feasible as an opt-in estimate, not transparent daemon telemetry.**
The XDG ScreenCast portal provides authorized PipeWire capture of a selected
monitor/window/virtual source; source selection normally presents a dialog and
permissions are portal-governed. [5] A dormant prototype may downscale samples
into a configurable grid, accumulate luminance × visible time, discard frames
immediately, and retain only explainable per-cell aggregates.

Android's shipped burn-in protection shifts content by 2-pixel steps rather than
keeping an exposure map, supporting a split between mitigation and optional
privacy-sensitive estimation. [7]

## Analysis

1. Do not replace uniform wear attribution with an imagined panel counter;
   model internal telemetry as unavailable rather than zero. [2]
2. Add a narrow `PanelTelemetry` discovery layer that persists raw read-only VCP
   results with EDID/model/firmware and evidence level. Promote semantics only
   after reproducible target observation plus independent evidence.
3. Keep `0xC0` as aggregate hours for calibrating wall-clock persistence, not
   spatial distribution.
4. Make frame sampling opt-in, visible, local-only, and OS-authorized. Never
   retain screenshots, raw frames, app/window names, or transmit them.
5. Keep manual colorimeter measurement separate from automatic telemetry. [6]

## Sources

1. [ddcutil FAQ](https://www.ddcutil.com/faq/) — capability-string caveats and
   safe probe semantics (accessed 2026-07-29).
2. [AOC AG326UD OLED Monitor User Manual](https://www.aoc.com/api/asset/pi34959?ext=pdf)
   — related-model OSD care counters and DDC/CI toggle (accessed 2026-07-29).
3. [Samsung Consumer IP Control Worksheet](https://image-us.samsung.com/SamsungUS/samsungbusiness/tv-ci-resources/Samsung-IP-Control.pdf)
   — consumer control material (accessed 2026-07-29).
4. [Samsung MDC Protocol 2015 v13.7c](https://aca.im/driver_docs/Samsung/MDC%20Protocol%202015%20v13.7c.pdf)
   — commercial-display protocol, not S90D evidence (accessed 2026-07-29).
5. [XDG Desktop Portal ScreenCast v6](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
   — user-authorized PipeWire capture API (accessed 2026-07-29).
6. [DisplayCAL](https://displaycal.net/) — instrument-backed uniformity
   measurements (accessed 2026-07-29).
7. [AOSP BurnInProtectionHelper](https://android.googlesource.com/platform/frameworks/base/+/master/services/core/java/com/android/server/policy/BurnInProtectionHelper.java)
   — shipped pixel-shifting implementation (accessed 2026-07-29).

## Claims

```yaml
- id: C1
  claim: "ddcutil treats capability strings as informational and recommends getvcp/setvcp testing."
  source: https://www.ddcutil.com/faq/
  quote: "The only way to know for sure if a monitor supports a VCP Feature Code is by testing using the getvcp and setvcp commands."
  load-bearing: yes
- id: C2
  claim: "The related AOC AG326UD OSD documents Time after Pixel Refresh and Pixel Refresh Counts."
  source: https://www.aoc.com/api/asset/pi34959?ext=pdf
  quote: "It refers to the time that the screen lights up after the last Pixel Refresh operation... It is used to record the number of times of executing Pixel Refresh."
  load-bearing: yes
- id: C3
  claim: "The related AOC manual presents DDC/CI as an OSD enable/disable setting, not an OLED-care mapping."
  source: https://www.aoc.com/api/asset/pi34959?ext=pdf
  quote: "DDC/CI Yes or No — Turn On/Off DDC/CI Support."
  load-bearing: yes
- id: C4
  claim: "Samsung MDC defines Maintenance Control separately from consumer IP-control documentation."
  source: https://aca.im/driver_docs/Samsung/MDC%20Protocol%202015%20v13.7c.pdf
  quote: "2.1.08 Maintenance Control"
  load-bearing: yes
- id: C5
  claim: "The XDG ScreenCast portal provides capture and starts through source selection."
  source: https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
  quote: "Start the screen cast session. This will typically result the portal presenting a dialog letting the user do the selection set up by SelectSources."
  load-bearing: yes
- id: C6
  claim: "Android burn-in protection shifts display offsets rather than maintaining an exposure map."
  source: https://android.googlesource.com/platform/frameworks/base/+/master/services/core/java/com/android/server/policy/BurnInProtectionHelper.java
  quote: "private static final int BURN_IN_SHIFT_STEP = 2;"
  load-bearing: no
```
