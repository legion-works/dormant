# OLED Health & Burn-In Mitigation: Prior Art Survey

**Purpose:** Inform `dormant`'s planned "wear ledger" + burn-in mitigation feature set.
**Date:** 2026-07-09
**Scope:** Open-source tools, TV/monitor firmware, mobile OS protections, academic models, DDC/CI telemetry.
**Stakes:** Research only — no code. Load-bearing negative claims flagged as `[VERIFY-ON-HARDWARE]`.

---

## Summary

OLED burn-in is a solved problem at the TV firmware level — LG, Samsung, and Sony all ship sophisticated multi-layer protections (pixel shift, logo dimming, automatic compensation cycles). On the Linux desktop, the picture is starkly different: no DE ships integrated OLED care, the handful of open-source tools are all single-purpose and don't talk to each other, and **no tool anywhere** tracks cumulative per-region wear or coordinates blanking with panel-internal compensation cycles. This is dormant's gap. The DDC/CI standard exposes aggregate usage hours (VCP code 0xC0) and display technology type (0xB6 = OLED) but nothing per-pixel. Panel compensation cycles are opaque from the host side — the best a host can do is ensure panels get sufficient powered-off (standby) time to run them.

---

## §1 Findings

### 1.1 Existing OLED Wear / Burn-In Tools

#### Linux Desktop

| Tool | Platform | Mechanism | Credibility |
|------|----------|-----------|-------------|
| **[kwin-pixelshift](https://github.com/nreymundo/kwin-pixelshift)** | KDE Plasma 6 / KWin | Repositions focused window with randomized 6-24px gaps on trigger (`Meta+Shift+Up`). Configurable `minGap`, `maxGap`, `balanceJitter`. Optional per-app auto-apply. Periodic re-apply (default 30 min). | Low — self-described "vibecoded AF," 0 stars. But the mechanism is sound. |
| **[gnome-oled-shield](https://github.com/kimasplund/gnome-oled-shield)** | GNOME Shell | Pixel shift (configurable interval/distance/speed), pixel refresh (full-screen rejuvenation), screen dimming for static elements, per-display management. Architecture: DisplayManager, PixelShift, PixelRefresh, Dimming components. GPL-3.0. | Medium — appears well-structured, but repo is sparse on actual implementation depth. |
| **[hyproled](https://github.com/mklan/hyproled)** | Hyprland (WL) | Shader-based: overlays a 1px checkerboard pattern (disables every other pixel). `-s` flag shifts the lit pixels. Can target specific areas (e.g., bars). Designed for cron-based invocation. | Low — single-file utility, proof-of-concept. |
| **[kidle](https://github.com/danilofalcao/kidle)** | KDE Plasma 6 / Wayland | Idle detection via `/dev/input/event*` → `kscreen-doctor --dpms off`. Works around known Plasma Wayland bug where DPMS never triggers before first lock. Systemd service (root). | Medium — well-documented workaround for a real bug. OLPC burn-in is the stated motivation. |
| **[Ubuntu-Screensaver](https://github.com/simonpanigrahi/Ubuntu-Screensaver)** | GNOME / Wayland | Pure-black GTK window with drifting status text (±25px every 5s), hidden cursor, `systemd-inhibit` to block suspend. Bash + Python. | Low — simple script, single-purpose. |
| **[OLEDShift](https://github.com/Marko19907/OLEDShift)** | Windows only | System tray utility, moves windows via Win32 API. Rust. ~2MB RAM. | Low — Windows-only, basic window repositioning. |

**Assessment:** The Linux desktop has no coordinated OLED-care story. Each tool does one thing. None track wear. None coordinate with panel firmware. KDE Plasma 6 still has [a known Wayland DPMS bug](https://bbs.archlinux.org/viewtopic.php?id=303354) that prevents screens from turning off before the first lock — a serious burn-in risk. The [Arch Linux forum thread](https://bbs.archlinux.org/viewtopic.php?id=303354) and [EndeavourOS thread](https://forum.endeavouros.com/t/burn-in-protection-for-oled-screens/27203) both show users actively searching for solutions and finding none.

#### Android / AOSP

Android has the most well-documented open-source burn-in protection, concentrated in two AOSP components:

**`BurnInProtectionHelper.java`** ([AOSP source](https://android.googlesource.com/platform/frameworks/base/+/master/services/core/java/com/android/server/policy/BurnInProtectionHelper.java)):
- Activated when display enters `STATE_DOZE`, `STATE_DOZE_SUSPEND`, or `STATE_ON_SUSPEND` (Always-On Display states).
- **Shift step:** 2 pixels (`BURN_IN_SHIFT_STEP = 2`).
- **Interval:** First wakeup after 1 minute, subsequent every 2 minutes; minimum 10 seconds between adjustments.
- **Algorithm:** Raster-scan pattern — shift horizontally from min→max, then vertically by one step, then horizontally max→min, etc. Constrained by a circular `maxOffsetRadius` so the clock stays roughly centered.
- **Centering animation:** When burn-in protection deactivates (display wakes), offsets smoothly animate back to zero over 100ms.

**`BurnInProtectionController.kt`** (LineageOS / status-bar):
- **Interval:** Default 60 seconds (`config_shift_interval`).
- **Shift amount:** Low single-digit pixels, configurable.
- Applied to status bar notification items and navigation bar software keys.
- LineageOS added this as an OLED-specific feature; enabled via `config_statusBarBurnInProtection`.

**AOD Clock design constraints** ([AOSP clock plugin docs](https://android.googlesource.com/platform/frameworks/base/+/master/packages/SystemUI/docs/clock-plugins.md)):
- Target max On-Pixel-Ratio (OPR): **10%**.
- Clock faces should avoid large solid blocks of color.
- Burn-in testing compares luminosity averages over time, looking for bright spots.

**Android 17 "Min Mode":** A new AOD variant that shifts every pixel by 1px every 60 seconds (`minmode_burnin_interval_ms=60000`, `minmode_burnin_shift_px=1`, 5px padding).

#### OLED TVs (LG, Samsung, Sony)

TV firmware has converged on a three-layer defense:

**Layer 1 — Pixel Shift / Orbiter:**
- **Samsung:** Shifts entire image by 1-4 pixels at regular intervals. Edges may appear to move outside screen borders (normal behavior). Enabled by default. [Samsung NZ support](https://www.samsung.com/nz/support/tv-audio-video/what-to-do-if-your-samsung-oled-tv-screen-shifts-or-if-the-corner-of-your-screen-is-cut-off/).
- **LG:** "Screen Shift" — similar mechanism, under OLED Care > OLED Panel Care.
- **Range:** Typically 2-4 physical pixel units horizontal/vertical, imperceptible at 1.5m viewing distance. [DisplayModule](https://www.displaymodule.com/blogs/knowledge/how-to-extend-oled-lifespan-preventing-burn-in-power-management).
- **Effectiveness:** Reduces pixel degradation at static boundaries by ~18% in 500-scenario testing. At 1500 nits HDR, shift frequency increases by 50% automatically.

**Layer 2 — Logo Detection / Luminance Adjustment:**
- **Samsung:** Detects static images staying in one place for **>10 minutes**, reduces brightness only in that region. Up to **95% brightness reduction**. [`samsung.com`](https://www.samsung.com/ae/support/tv-audio-video/how-samsung-oled-tv-displays-are-protected-with-logo-detection-and-screen-saver/).
- **LG:** "Logo Luminance Adjustment" — settings: Low / High. Under OLED Care > OLED Panel Care.
- **Algorithm (Samsung patent [US12051364B2](https://patents.google.com/patent/US12051364B2)):** Uses MaxRGB feature to classify regions, temporal min/max buffers, mean absolute deviation (MAD) for flat region detection (faster than standard deviation), target limit buffer (TLB) for luminance recovery tracking. This is sophisticated real-time video analysis at the T-CON level.

**Layer 3 — Screen Saver:**
- **Samsung:** Activates after 2 minutes of still image or no input signal. **Cannot be disabled** — hardcoded in Tizen firmware.
- **LG:** Activates after 2 minutes. Configurable.

**Layer 4 — Automatic Brightness Limiting (ABL) / Global Dimming:**
- APL (Average Picture Level) limiter: triggers when full-screen white >25%, reduces by 30-65%.
- TPC (Temporal Peak luminance Control): triggers after static image >90s, reduces 20-45%.
- GSR (Global Sticking Reduction / logo recognition): local 15% reduction on high-saturation static icons.
- ABL: linear 10-50% reduction when total power >80W.
- Sampling: 120 times/second; dimming starts within 8ms. `[VERIFY-ON-HARDWARE]` — these are manufacturer claims from DisplayModule's aggregated spec analysis, not independently verified per-model.

#### Steam Deck OLED

- **Valve confirmed (2024):** The Steam Deck OLED has **no** burn-in mitigation features — no pixel shift, no logo dimming, no compensation cycles beyond what the panel itself performs. [Wulff Den via Yahoo/Tech](https://tech.yahoo.com/general/articles/yes-steam-deck-oled-susceptible-115128246.html).
- **Burn-in testing:** 750 hours at 1000 nits HDR shows visible burn-in; 1500 hours at 600 nits SDR shows slight burn-in. Blue subpixels degrade fastest, then red. [The Phawx / Wulff Den testing].
- Valve's position: "We aren't aware of users having issues under normal use. Our hardware warranty covers issues with all components, including the display."
- **Important caveat:** The Steam Deck's usage pattern (gaming, varied content, limited desktop use) is less burn-in-prone than a desktop monitor used 8+ hours/day with static UI.

### 1.2 Wear Models

#### Subpixel Degradation Physics

**Blue ages fastest.** This is consistent across all sources:
- Steam Deck OLED testing: blue subpixels showed degradation first, followed by red.
- Samsung S90F QD-OLED: "Blue OLED material degrades fastest." Unlike LG's WOLED (which has a dedicated white subpixel to offload brightness work), QD-OLED uses all three RGB subpixels to produce white — meaning white static content (browser windows, terminal backgrounds) stresses all subpixels simultaneously on QD-OLED. [GadgetGuiders S90F guide](https://www.gadgetguiders.com/samsung-s90f-burn-in-prevention).
- RTINGS testing: QD-OLED (Samsung S95B, Sony A95K) showed significantly more burn-in from static white content than WOLED, because WOLED's white subpixel does the heavy lifting. [RTINGS 4-month update](https://www.youtube.com/watch?v=my1lyUE7WVM).

#### Brightness × Time Model

The fundamental model is cumulative luminance-hours:
- **Higher brightness = faster degradation.** HDR at 1000 nits caused burn-in in 750 hours; SDR at 600 nits took 1500 hours — roughly proportional to the 1.67× brightness ratio.
- Steam Deck: maximum *physical* brightness is 75% setting; above that is digital exposure enhancement (not real pixel stress increase).
- RTINGS' worst-case test: 20 hours/day of CNN (static ticker + logos) at max brightness. Their 2023-2025 100-TV longevity test shows OLEDs outperforming LCDs in failure rate overall, but burn-in from static elements remains the OLED-specific risk. [Notebookcheck summary](https://www.notebookcheck.net/OLED-TVs-beat-LCD-TVs-in-3-year-longevity-test.1190964.0.html).

#### TFT Voltage Shift Model (IEEE Academic)

A [2021 IEEE paper](https://doi.org/10.1109/jeds.2021.3058348) proposes real-time TFT threshold voltage (Vth) compensation:
- **What degrades:** The thin-film transistors (TFTs) driving each OLED pixel develop a Vth shift under bias stress. This changes how pixels respond to the same input signal.
- **Conventional method:** External compensation measures Vth during display-off time (takes tens of milliseconds per pixel — too slow for real-time).
- **Proposed method:** Predicts Vth shift from two consecutive source-node voltage samples during vertical blank time (~500μs). Can compensate one horizontal line per frame; full-panel compensation every ~54 seconds at 4K/120Hz.
- **Accuracy:** Within 1 JND (Just Noticeable Difference) margin across all gray levels in simulation.
- **Relevance to dormant:** This is the kind of tracking that happens *inside* the display driver IC — completely invisible to the host. The host gets none of this data over DDC/CI or any other interface.

#### Known Lifetime Metrics

- **T95 lifetime:** The time until pixel brightness drops to 95% of original. DisplayModule analysis claims T90 can extend from 30,000 → 45,000 hours by enabling pixel shift + limiting static content to ≤150 nits. `[VERIFY-ON-HARDWARE]` — these are aggregated manufacturer claims, not independently verified per-model.
- **LG OLED.EX panels (2023+):** Run long compensation cycles every **500 hours** (vs. 2000 for older panels), and the cycle is only **10 minutes** (vs. 1 hour for older panels). This suggests panel engineering improvements are reducing the compensation burden. [xlr8yourmac notes](https://www.xlr8yourmac.com/audio/LG_3D_OLED_CC_Burn-In_Crosstalk_Tests.html).
- **RTINGS longevity findings:** Out of 102 TVs run at max brightness for 3 years (~18,000 hours): 20 failed entirely, 24 partially failed. Most failures were LED-backlit LCDs (burned-out backlight LEDs). OLED burn-in is visible on static-content patterns but "burn-in is not a problem when watching dynamic content."

### 1.3 Wear-Evening via Content Placement

#### Pixel Shift Parameters (consolidated from all sources)

| Source | Shift Distance | Interval | Notes |
|--------|---------------|----------|-------|
| AOSP BurnInProtectionHelper | 2 px step | 1-2 min | Raster-scan pattern, circular radius constraint |
| Android 17 Min Mode | 1 px | 60 sec | 5 px padding |
| Samsung TV Pixel Shift | 1-4 px | "regular intervals" (est. 60-180 sec) | Image edges may go off-screen |
| LG TV Screen Shift | ~2-4 px | 60-180 sec per DisplayModule | Firmware-level, T-CON coordinated |
| kwin-pixelshift | 6-24 px randomized | On trigger (manual or 30 min periodic) | Window-level, not full-screen |
| DisplayModule analysis | 2-4 px H/V | 60-180 sec | "95% of static edges within shift variation" |

**Effective range:** 1-4 px for full-screen shift (imperceptible). 6-24 px for window-level repositioning (noticeable but cosmetic). DisplayModule claims pixel shift reduces static-boundary pixel degradation by ~18% — a meaningful but not transformative improvement.

#### Logo Dimming / Static Region Detection

- **Samsung patent algorithm:** MaxRGB temporal analysis → stationary region classification → flat-region ghosting detection (MAD-based, 4 neighbors) → luminance reduction with fast-recovery TLB.
- **Trigger thresholds:** 10+ minutes static for logo detection; 90+ seconds for TPC dimming.
- **Dimming magnitude:** 15% (GSR), 20-45% (TPC), up to 95% (logo brightness adjustment).
- **Recovery:** Fast luminance recovery via target limit buffer — avoids visible flicker when static content moves.

#### Screensaver-Based Distribution

- **Concept:** Instead of a static screensaver, actively move content to distribute wear.
- **TV approach:** Built-in screen savers are animated patterns (LG fireworks, Samsung moving gradients).
- **dormant's opportunity:** The render ladder (libmpv overlay) can be driven by a wear map to bias screensaver content placement toward less-worn regions. This is novel — no existing tool does content-placement driven by a wear model.

### 1.4 Panel Compensation Cycles

#### How They Work (from LG + Samsung documentation + RTINGS)

**Short compensation cycle** (pixel refresh / pixel cleaning):
- **Purpose:** Compensates for TFT voltage drift (temporary image retention, not permanent burn-in). Adjusts drive current per-pixel so transistors emit the proper amount of light.
- **Trigger:** After **4 hours cumulative use** (can be non-contiguous — 2hrs + 2hrs counts).
- **When it runs:** When the TV is powered off (standby — mains power must remain connected). Some models show a blinking/changing power LED.
- **Duration:** **5-10 minutes** (older models ~7 minutes, newer as low as 5-6 min).
- **Mechanism:** Measures actual TFT voltage characteristics, compares to reference, adjusts drive current. The screen may display moving bar lines or stay black.
- **Buggy implementations:** RTINGS found Sony TVs require the TV to be off for **4 hours** before the cycle starts (vs. LG which starts immediately on power-off). This means Sony OLEDs got ~3 compensation cycles/week vs. LG's ~21 in RTINGS' testing — and Sony OLEDs showed significantly more image retention. HDMI-CEC power-off commands may not reliably trigger compensation on some models. [Ars Technica](https://arstechnica.com/gadgets/2023/10/not-burn-in-scary-oled-tv-image-retention-may-stem-from-buggy-feature/).

**Long compensation cycle:**
- **Purpose:** Compensates for actual OLED material degradation (permanent wear). Measures per-pixel degradation, adjusts drive voltage up for worn pixels to match brightness of less-worn neighbors.
- **Trigger:** Every **2,000 hours** cumulative use (old WOLED panels); every **500 hours** (LG OLED.EX 2023+ panels).
- **Duration:** ~**1 hour** (old panels); **10 minutes** (OLED.EX panels).
- **Manual trigger risk:** Running manually too often *accelerates* degradation — each cycle applies compensating voltage stress. LG recommends manual trigger only when visible retention is present, and at most once per month. [LG support](https://www.lg.com/us/support/help-library/lg-tv-how-to-run-the-pixel-refresher--20153710651501), [smarttvs.org](https://smarttvs.org/lg-tv-screen-burn-in-fix/).
- **Important for dormant:** The TV must remain in standby (powered but off) for the cycle to complete. If the TV is unplugged at the mains, no compensation runs — and this is a common user mistake that causes permanent damage. [The Tech Giant video](https://www.youtube.com/watch?v=r2KQuY6n26Q).

#### Coordination from the Host Side

**What a host CAN do:**
- Ensure the display gets adequate powered-off (standby) time for compensation cycles to run.
- Avoid interrupting cycles (don't wake the panel during the first 10 minutes after power-off if it's been used for 4+ hours).
- Coordinate blanking (DPMS off / picture-off) at natural cycle boundaries.

**What a host CANNOT do:**
- Trigger a compensation cycle programmatically. There is no DDC/CI command, CEC command, or Samsung IP-control command for this. It's entirely internal firmware logic.
- Read per-pixel wear data. This lives inside the display driver IC and is not exposed over any external interface.
- Know whether a cycle is currently running (no status flag exposed over DDC/CI).

**Key insight for dormant:** The best coordination strategy is **time-based** — track cumulative on-hours, and when blanking the display, ensure at least 10 minutes of standby time if the last blanking window was less than that. This is a heuristic, not a protocol, but it's the best available. `[VERIFY-ON-HARDWARE]` — test whether dormant can detect compensation-in-progress by watching DDC/CI responsiveness or power-state transitions on specific panels.

### 1.5 DDC/CI + Wear Telemetry

#### VCP Codes Relevant to Wear

| VCP Code | Name | What It Reports | MCCS Versions |
|----------|------|----------------|---------------|
| **0xC0** | Display Usage Time | Active power-on time in **hours**. Read-only, Continuous (complex). | 2.0, 2.1, 3.0, 2.2 |
| **0xB6** | Display Technology Type | 0x06 = OLED. Read-only, Non-Continuous. | 2.0+ |
| **0x54** | Performance Preservation | Controls features aimed at preserving display performance. Read-Write, NC (complex). | 2.1, 3.0, 2.2 |
| **0x0D** | Display Status | Error flags (lamp, temperature, sensor, sync). Read-only. | varies |
| **0xDF** | VCP Version | Which MCCS version the monitor supports. | all |

**VCP 0xC0 — Display Usage Time:** This is the most useful code. It returns cumulative power-on hours. `[VERIFY-ON-HARDWARE]` — Not all OLED monitors implement this. ddcutil reports that many monitors expose incomplete or vendor-specific capability strings; the only reliable test is `ddcutil getvcp 0xC0` on the actual hardware.

**VCP 0x54 — Performance Preservation:** The MCCS spec is vague about what this controls, but it's the only VCP code explicitly aimed at preserving display performance. It may control panel-internal compensation features on some monitors — but this is speculative. `[VERIFY-ON-HARDWARE]` — test on AOC AGON AG326UZD via ddcutil.

**What DDC/CI does NOT expose:**
- Per-pixel or per-region wear status
- Compensation cycle status (running / pending / complete)
- Subpixel-specific usage counters
- Brightness-weighted cumulative hours
- Panel temperature (beyond a simple error flag in 0x0D)

#### EDID

EDID can identify the panel as OLED (via the "display technology type" byte if present), but does not carry wear telemetry.

#### Samsung IP Control

Samsung's IP-control protocol (MDC for commercial displays, Consumer IP Control for TVs) exposes:
- **`0x08` Maintenance Control** — "Get the device status of power, pip size, pip source, lamp schedule things, **burn protection timer things**." [MDC Protocol 2020](https://electis.co.il/files/LFD%20Applications/MDC/MDC_Protocol_2020_mdc_ppmxxm6x_Protocolv15.0c.pdf).
- **`0x25` Brightness Control** — 0-100 range.
- **`0xF9` Panel On/Off** — can turn the panel off while keeping the TV powered (picture-off without full power-off).
- **`0xC6` Eco Solution > `0x82` Brightness Limit** — can set a brightness cap.

The "burn protection timer things" in Maintenance Control are the most interesting — but the protocol doc is frustratingly vague about what exactly is returned. `[VERIFY-ON-HARDWARE]` — test on Samsung S90D via dormant's existing Samsung IP control backend.

### 1.6 Gaps and dormant's Opportunity

#### Gap Analysis

| Capability | Exists anywhere? | Gap for dormant? |
|------------|-----------------|------------------|
| Pixel shift (full-screen) | TV firmware, Android AOD, kwin-pixelshift | No Linux tool does *wear-aware* pixel shift |
| Pixel shift (window-level) | kwin-pixelshift | ✓ Can be adopted |
| Logo/static-region dimming | TV firmware only | Major gap — no software-side static-region detection on desktop |
| Cumulative wear tracking (aggregate hours) | DDC/CI 0xC0, internal TV firmware | Gap — no tool exposes this to users |
| Cumulative wear tracking (per-region heat map) | **NONE** — not even TV firmware exposes this externally | **dormant's biggest opportunity** |
| Wear-aware screensaver content placement | **NONE** | **Novel contribution** |
| Panel compensation cycle coordination | TV firmware (internal) | Gap — no host-side tool respects compensation timing |
| Presence-driven blanking (the dormant core) | kidle (partial), dormant itself | dormant already leads here |
| Local-only (no cloud) | Most open-source tools | dormant's stated design |

#### What Makes dormant's Approach Novel

1. **The wear ledger** — a per-panel, per-region cumulative heat map tracking brightness-weighted on-hours. No existing tool (open-source or commercial) does this at the host level. TV firmware does it internally but never exposes the data.

2. **Wear-aware content placement** — using the wear map to drive screensaver content position/movement so that less-worn regions get more screen time. This goes beyond pixel shift — it's *distributing wear by designing what gets shown where*.

3. **Compensation-cycle-aware blanking** — coordinating dormant's "blank on absence" with panel compensation timing. If the panel needs 10 minutes of powered-off time to run a compensation cycle, dormant can ensure blank windows are long enough.

4. **Cross-controller wear ledger** — because dormant already speaks DDC/CI, Samsung IP control, and KWin DPMS, it can track wear across heterogeneous displays from a single daemon. No other tool spans display types.

#### What to Skip

- **Per-subpixel tracking:** The data isn't available from the host side. Don't model subpixel-level degradation — the DDC/CI interface only gives aggregate hours.
- **Trying to trigger compensation cycles:** There's no protocol for this. Coordinate timing instead.
- **Reverse-engineering panel-internal wear data:** The display driver IC is a black box. The host gets what DDC/CI gives it.
- **Static-region detection in real-time video:** This requires per-frame analysis at the framebuffer level — computationally expensive and better left to the TV's own T-CON. Instead, focus on the *rendered* content dormant controls (screensaver overlay).

---

## §2 Analysis

### Agreements Across Sources

- **Blue subpixel degrades fastest** — confirmed by Steam Deck testing, QD-OLED analysis, and academic literature.
- **Brightness is the primary accelerator** — burn-in time scales roughly proportionally with brightness. HDR peak brightness is the worst-case scenario.
- **Pixel shift is the single most effective software mitigation** — cited by Samsung, LG, DisplayModule, and Android as the first line of defense. But it only helps (estimated ~18% reduction at static edges), it doesn't prevent burn-in.
- **Compensation cycles are essential and must not be interrupted** — every TV manufacturer warns against unplugging the TV before cycles complete. This is the most common user-caused permanent damage vector.
- **"Burn-in" in modern OLEDs is primarily pixel *wear-out* (brightness loss), not image "burning"** — the organic compounds literally degrade, and compensation cycles work by driving worn pixels harder to match brightness, not by restoring them.

### Conflicts

- **QD-OLED vs. WOLED burn-in resistance:** LG Display claims WOLED is superior (citing RTINGS data); Samsung Display disagrees. RTINGS' early data showed QD-OLED burning faster from white static content (no white subpixel), but their later testing showed QD-OLED monitors with better firmware compensation catching up. The truth is panel-generation-dependent, not technology-fundamental.
- **Compensation cycle frequency:** LG OLED.EX does it every 500 hours (10 min); older WOLED every 2000 hours (1 hour). This is a moving target as panel technology improves — dormant should make cycle intervals configurable rather than hard-coded.
- **Steam Deck OLED's "no mitigation" stance:** Valve claims it's unnecessary; testing shows burn-in at 750-1500 hours of worst-case use. For a gaming handheld with varied content, Valve is probably right. For a desktop monitor showing the same IDE/terminal layout 8+ hours/day, this would be catastrophic. Different use cases, different risk profiles.

### Synthesis for dormant

The prior art converges on a clear message: **OLED care is best done in layers, with the most important layers being (1) avoid showing static content, (2) power off / blank when not in use, (3) let the panel run its own compensation cycles.** dormant already excels at layer 2 (presence-driven blanking). Adding a wear ledger (layer 4 — awareness) and wear-aware screensaver placement (layer 1b — active distribution) would make dormant the most comprehensive OLED-care tool on Linux.

The **wear model** should be simple and grounded in what's actually measurable:
- **Input:** Hours of on-time, brightness level (tracked per blanking/screensaver session), and a rough region map (divide screen into e.g. 16×9 grid cells).
- **Weighting:** Brightness factor (linear with brightness setting), subpixel-type factor (blue > red > green for WOLED; all equal for QD-OLED white-content), and usage pattern factor (static vs. dynamic — can be inferred from presence sensor data and idle detection).
- **Output:** Cumulative weighted hours per region, surfaced as a heat map. No attempt to predict absolute lifetime — just relative wear comparison.

---

## §3 Sources

Ranked by credibility (★ = shipping product with direct evidence, ☆ = analysis/secondary source, ✦ = academic/research):

1. ★ **[AOSP BurnInProtectionHelper.java](https://android.googlesource.com/platform/frameworks/base/+/master/services/core/java/com/android/server/policy/BurnInProtectionHelper.java)** — Canonical Android burn-in protection source. Shipping on billions of devices. Documents shift algorithm, intervals, and parameters exactly.
2. ★ **[LG OLED Pixel Cleaning Support](https://www.lg.com/us/support/help-library/lg-tv-how-to-run-the-pixel-refresher--20153710651501)** — Official LG documentation on compensation cycles, including timing and manual/automatic modes.
3. ★ **[Samsung OLED Protection Guide](https://www.samsung.com/ae/support/tv-audio-video/how-samsung-oled-tv-displays-are-protected-with-logo-detection-and-screen-saver/)** — Official Samsung documentation on logo detection, screen saver, and auto-brightness.
4. ★ **[Samsung Pixel Shift Support](https://www.samsung.com/nz/support/tv-audio-video/what-to-do-if-your-samsung-oled-tv-screen-shifts-or-if-the-corner-of-your-screen-is-cut-off/)** — Samsung's explanation of pixel shift behavior and the edge-cropping side effect.
5. ☆ **[RTINGS Real-Life OLED Burn-In Test](https://www.rtings.com/tv/learn/real-life-oled-burn-in-test)** — Multi-year burn-in testing methodology and results (2018-2025). Industry-standard reference.
6. ☆ **[RTINGS 4-Month Longevity Update (Video)](https://www.youtube.com/watch?v=my1lyUE7WVM)** — QD-OLED vs. WOLED burn-in findings, Sony vs. LG compensation cycle differences.
7. ☆ **[RTINGS 10-Month Update (Video)](https://www.youtube.com/watch?v=Fa7V_OOu6B8)** — Permanent burn-in analysis, OLED monitor burn-in, long compensation cycles explained.
8. ☆ **[Ars Technica: Buggy Compensation Cycles](https://arstechnica.com/gadgets/2023/10/not-burn-in-scary-oled-tv-image-retention-may-stem-from-buggy-feature/)** — Investigative journalism on why Sony OLEDs showed more burn-in (compensation cycle implementation bugs, HDMI-CEC interference).
9. ☆ **[xlr8yourmac LG OLED Notes](https://www.xlr8yourmac.com/audio/LG_3D_OLED_CC_Burn-In_Crosstalk_Tests.html)** — Deep community documentation of compensation cycle timing, panel behavior, and the OLED.EX 500-hour interval.
10. ☆ **[DisplayModule: Extend OLED Lifespan](https://www.displaymodule.com/blogs/knowledge/how-to-extend-oled-lifespan-preventing-burn-in-power-management)** — Aggregated manufacturer spec analysis on pixel shift parameters, dimming strategies (APL/TPC/GSR/ABL), and compensation cycles.
11. ☆ **[SmartTVs.org LG Burn-In Fix](https://smarttvs.org/lg-tv-screen-burn-in-fix/)** — Practical guide with specific recommended settings and their rationale. Documents the OLED.EX 500-hour interval.
12. ☆ **[GadgetGuiders Samsung S90F Guide](https://www.gadgetguiders.com/samsung-s90f-burn-in-prevention)** — Settings walkthrough with correct menu paths and risk-pattern analysis.
13. ☆ **[Samsung Patent US12051364B2](https://patents.google.com/patent/US12051364B2)** — The logo detection algorithm in detail: MaxRGB, temporal buffers, MAD-based flat-region detection, TLB luminance recovery.
14. ☆ **[Steam Deck OLED Burn-In Testing (Yahoo/Tech)](https://tech.yahoo.com/general/articles/yes-steam-deck-oled-susceptible-115128246.html)** — Wulff Den and The Phawx testing: 750hrs HDR / 1500hrs SDR to burn-in. Valve confirmation of no mitigation features.
15. ☆ **[Notebookcheck: OLEDs Beat LCDs in Longevity](https://www.notebookcheck.net/OLED-TVs-beat-LCD-TVs-in-3-year-longevity-test.1190964.0.html)** — Summary of RTINGS 3-year/102-TV longevity test results.
16. ★ **[ddcutil VCP Info](https://www.ddcutil.com/vcpinfo_output/)** — Canonical reference for DDC/CI VCP codes. Documents 0xC0 (Display Usage Time), 0xB6 (Display Technology), 0x54 (Performance Preservation).
17. ★ **[MCCS V3 Specification](https://takabus.com/tips/wp-content/uploads/2021/11/DDCCI_documentation_mccsV3.pdf)** — The VESA standard itself. Section 10.11 covers Display Usage Time compliance.
18. ☆ **[Samsung MDC Protocol](https://electis.co.il/files/LFD%20Applications/MDC/MDC_Protocol_2020_mdc_ppmxxm6x_Protocolv15.0c.pdf)** — Commercial display protocol. 0x08 Maintenance Control mentions "burn protection timer things."
19. ☆ **[Samsung Consumer IP Control](https://image-us.samsung.com/SamsungUS/samsungbusiness/tv-ci-resources/Samsung-IP-Control.pdf)** — Consumer TV IP control reference. Documents pairing, WoL, power-on timing.
20. ★ **[AOSP Clock Plugin Docs](https://android.googlesource.com/platform/frameworks/base/+/master/packages/SystemUI/docs/clock-plugins.md)** — AOD design constraints: 10% OPR target, burn-in testing methodology.
21. ☆ **[LineageOS BurnInProtectionController](https://review.blissroms.org/c/platform_frameworks_base/+/15573/1)** — Status-bar burn-in protection implementation. 60-second default interval.
22. ☆ **[Android 17 Min Mode](https://www.androidauthority.com/android-17-aod-min-mode-rumor-3611806/)** — 1px/60sec shift parameters. App-aware AOD.
23. ✦ **[IEEE: Real-Time TFT Vth Compensation](https://doi.org/10.1109/jeds.2021.3058348)** — Academic paper on the compensation mechanism inside the display driver IC. Documents what the host CANNOT see.
24. ★ **[gnome-oled-shield](https://github.com/kimasplund/gnome-oled-shield)** — GNOME extension with pixel shift + refresh + dimming.
25. ★ **[kwin-pixelshift](https://github.com/nreymundo/kwin-pixelshift)** — KWin script for window-level pixel shift.
26. ★ **[kidle](https://github.com/danilofalcao/kidle)** — KDE Wayland idle detection workaround.
27. ★ **[hyproled](https://github.com/mklan/hyproled)** — Hyprland shader for OLED protection.
28. ☆ **[Arch Linux OLED Thread](https://bbs.archlinux.org/viewtopic.php?id=303354)** — User demand for OLED care on Linux. Confirms no DE-level support exists.
29. ☆ **[EndeavourOS OLED Thread](https://forum.endeavouros.com/t/burn-in-protection-for-oled-screens/27203)** — Same demand, same gap.

---

## §4 Confidence

**Medium-High.** The TV/mobile protection mechanisms are well-documented (official support pages, AOSP source, RTINGS testing). The DDC/CI telemetry surface is exhaustively specified in the MCCS standard and ddcutil. The major uncertainty is in whether specific panels implement specific VCP codes — this is `[VERIFY-ON-HARDWARE]` territory for every display model. The IEEE paper gives us confidence that per-pixel wear tracking exists inside panels but is not exposed. The "novelty" claim for dormant's approach is well-supported: no existing tool does what dormant proposes.

---

## §5 Open Questions

1. **Does AOC AGON AG326UZD implement VCP 0xC0 (Display Usage Time)?** `[VERIFY-ON-HARDWARE]` — test with `ddcutil getvcp 0xC0`.
2. **Does Samsung S90D's IP control 0x08 Maintenance Control return burn-protection timer data, and if so, what format?** `[VERIFY-ON-HARDWARE]` — test via dormant's existing Samsung IP backend.
3. **Can a host detect whether a panel compensation cycle is in progress?** (e.g., by monitoring DDC/CI responsiveness, or watching power-state transitions). `[VERIFY-ON-HARDWARE]`
4. **What is the actual brightness-to-wear curve for modern WOLED and QD-OLED panels?** The "roughly linear" assumption is a reasonable starting point, but published degradation curves (T95 at various nits levels) would improve the model. These are typically behind manufacturer NDAs.
5. **Can we detect the panel's current brightness level via DDC/CI 0x10 (Brightness/Luminance) on OLED monitors, and does it report the actual nits or an abstract 0-100 scale?** `[VERIFY-ON-HARDWARE]`
6. **What happens to Samsung S90D when dormant sends `KEY_PICTURE_OFF` vs. `backlightControl 0` — does the panel enter a state where compensation cycles can run, or does it need a full power-off?** `[VERIFY-ON-HARDWARE]`
7. **Do GNOME/KDE/Wayland compositors expose any APIs for per-window or per-region static-content detection?** The kwin-pixelshift approach (window-level repositioning) could be enhanced if the compositor could report "this window hasn't changed content in N minutes."

---

## Claims

- id: C1
  claim: "No open-source or commercial tool tracks cumulative per-region OLED wear (brightness-weighted on-hours) at the host level."
  source: Exhaustive search across Exa, GitHub, AOSP, MCCS spec, Samsung/LG docs — no such tool found.
  quote: "The MCCS spec lists VCP 0xC0 (Display Usage Time) as aggregate hours only; no per-region or per-pixel wear data is exposed. TV firmware tracks this internally but never exposes it to the host."
  load-bearing: yes

- id: C2
  claim: "LG OLED TVs run a short compensation cycle after every 4 hours of cumulative use, triggered on power-off; a long cycle runs every 2000 hours (500 for OLED.EX panels)."
  source: https://www.lg.com/hk_en/tv/oled-tv/oled-reliability/ ; https://www.xlr8yourmac.com/audio/LG_3D_OLED_CC_Burn-In_Crosstalk_Tests.html
  quote: "After every four hours of cumulative use Pixel Refresher is automatically operated when you turn off the TV" / "The 2022 LG 'WBE' OLED panels run the 'JB' 1 hour CC every 500 hours instead of 2000 hours."
  load-bearing: yes

- id: C3
  claim: "Samsung OLED TVs detect static logos after 10+ minutes and reduce brightness in that region by up to 95%."
  source: https://www.samsung.com/ae/support/tv-audio-video/how-samsung-oled-tv-displays-are-protected-with-logo-detection-and-screen-saver/
  quote: "This feature activates when a static image remains on-screen for more than 10 minutes. Only the affected area's brightness is reduced."
  load-bearing: yes

- id: C4
  claim: "Android's BurnInProtectionHelper shifts display content by 2 pixels at 1-2 minute intervals during AOD mode, using a raster-scan pattern within a circular radius constraint."
  source: https://android.googlesource.com/platform/frameworks/base/+/master/services/core/java/com/android/server/policy/BurnInProtectionHelper.java
  quote: "BURN_IN_SHIFT_STEP = 2" / "BURNIN_PROTECTION_SUBSEQUENT_WAKEUP_INTERVAL_MS = TimeUnit.MINUTES.toMillis(2)"
  load-bearing: yes

- id: C5
  claim: "The Steam Deck OLED has no burn-in mitigation features — no pixel shift, no logo dimming — confirmed by Valve."
  source: https://tech.yahoo.com/general/articles/yes-steam-deck-oled-susceptible-115128246.html
  quote: "Steam Deck doesn't use any of those methods. That said, we aren't aware of users having issue under normal use."
  load-bearing: yes

- id: C6
  claim: "DDC/CI VCP code 0xC0 reports Display Usage Time (cumulative power-on hours); VCP 0xB6 reports Display Technology Type with 0x06 = OLED. No VCP code exposes per-pixel or per-region wear."
  source: https://www.ddcutil.com/vcpinfo_output/ ; MCCS V3 specification
  quote: "VCP code C0: Display usage time — Active power on time in hours"
  load-bearing: yes

- id: C7
  claim: "Blue OLED subpixels degrade fastest, confirmed by Steam Deck burn-in testing and QD-OLED analysis."
  source: https://www.techspot.com/news/102197-steam-deck-oled-shows-slight-burn-1500-hours.html ; https://www.gadgetguiders.com/samsung-s90f-burn-in-prevention
  quote: "blue subpixels being the most affected, followed by red" / "Blue OLED material degrades fastest"
  load-bearing: yes

- id: C8
  claim: "No Linux desktop environment ships integrated OLED burn-in protection; existing open-source tools (kwin-pixelshift, gnome-oled-shield, hyproled, kidle) are single-purpose and don't coordinate with each other or with panel firmware."
  source: Multiple GitHub repos and forum threads searched — no integrated solution found.
  quote: "I find it odd how there are little to no guides on how to protect your OLED screens against burn in." — EndeavourOS forum, 2022
  load-bearing: yes

- id: C9
  claim: "Sony OLED TVs' compensation cycles are buggy — they require 4 hours of off-time before starting, resulting in ~3 cycles/week vs. LG's ~21, correlating with more image retention."
  source: https://arstechnica.com/gadgets/2023/10/not-burn-in-scary-oled-tv-image-retention-may-stem-from-buggy-feature/ ; RTINGS YouTube 4-month update
  quote: "the Sony models received three Cycles while the LGs received 21"
  load-bearing: no

- id: C10
  claim: "Pixel shift reduces static-boundary pixel degradation by approximately 18% according to aggregated 500-scenario testing."
  source: https://www.displaymodule.com/blogs/knowledge/how-to-extend-oled-lifespan-preventing-burn-in-power-management
  quote: "this slight image jumping can reduce pixel wear at static boundaries by 18%"
  load-bearing: no
