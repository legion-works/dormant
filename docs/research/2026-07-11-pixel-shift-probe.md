# Pixel-shift mechanism probe — findings (OLED-health T9)

Date: 2026-07-11 · Hardware: AOC AGON AG326UZD (DP-1, 3072×1728 buffer scale, output scale 1.25) · Compositor: KWin 6.7.2 Wayland · Probe code: `~/projects/oled-proximity-spike/pixel-shift/` (spike quarantine, not in repo)

## Decision

**Mechanism: `wp_viewport` source-rect shift.** First preference in the spec's order, passed every GO criterion on the live session. The mpv `video-pan` fallback is viable on the wire but is not needed.

## Probe 1 — viewport source-rect walk (the winner)

Layer-shell Overlay surface on DP-1, wl_shm checkerboard buffer at (width+8, height+8), `wp_viewport::set_source(x, y, w, h)` stepped through a 2px raster walk every 2s, `set_destination(w, h)` fixed. Two full runs, ~30s each.

| GO criterion | Result | Evidence |
|---|---|---|
| Visibly translates content by 2px | **PASS** | Operator confirmed visible stepping; screenshot cross-correlation: consecutive captures match at exactly the applied raster offset (best-fit shift (2–3, 0) physical px, mean-abs-diff 1.49 vs 25+ unshifted) |
| No seam | **PASS** | Right/bottom 5px edge strips of captures contain only checkerboard greys (119–180), zero black/garbage slivers — the 8px margin crop holds to the last pixel |
| No protocol error | **PASS** | Two clean runs, zero errors, clean teardown; `set_source` steps accepted at 2s cadence |
| No compositor stutter | **PASS** | Operator observed clean instant steps, no smearing/flicker |

### Findings that shape T10

1. **Damage + commit per step**: each `set_source` change requires `wl_surface.damage_buffer` + `commit` to take effect — the probe drives exactly that; no full re-render, no new buffer attach. Cost per shift is effectively zero (compositor-side crop move).
2. **Fractional scale multiplies the physical step**: the output runs scale 1.25, so a 2-buffer-px shift lands as ~2.5 physical px (measured 2–3px in captures). Acceptable for wear-evening (the shift magnitude is approximate by design); T10 should document that `shift_px` is in buffer pixels.
3. **Oversized-buffer margin works as designed**: render at `(w + 2·margin, h + 2·margin)`, walk the source rect inside the margin. Margin must be ≥ the raster walk's max offset (probe used 8px for a 4px max walk — 2× headroom).
4. **Scale events**: the probe saw integer `wl_output` scale 2 alongside the real 1.25 fractional scale — legacy integer-scale events round up. T10 must not derive geometry from the integer scale event (use configure dimensions, as the probe and dormant-render already do).

## Probe 2 — mpv `video-pan` (fallback, documented only)

Windowed mpv + IPC socket, `video-pan-x/y` stepped in ~2px-equivalent increments (0.0016/step at 1280w), two passes: `--panscan=1` (fill) and `--panscan=0 --keepaspect` (fit). Every IPC step accepted (`"error":"success"` × 20, both passes). Visual observation was not captured (operator engaged elsewhere during the run) — moot for the decision since viewport passed, but a re-run is 60s (`bash probe-pan.sh`) if fill/fit clamp behavior ever matters.

## T10 implications

- Implement the shift in `dormant-render`'s existing surfaces (black + screensaver): allocate the buffer with the configured margin, drive the raster walk on a `shift_interval` timer via `set_source` + damage + commit. Config keys `shift_px` / `shift_interval` already exist (validated, no consumer yet).
- The screensaver path renders via mpv into our buffer — the viewport shift composes on top without touching the mpv pipeline (no `video-pan` needed).
- Teardown/re-show already reconfigure the viewport; the shift state just resets with the surface (no persistence needed).
