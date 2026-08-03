# Two-stream active sampling — hardware gate

Status: **scaffold — awaiting measurement**. Every measured cell below is TBD
until the deferred two-stream hardware gate runs on the maintainer's hardware
(AOC AGON AG326UZD over DisplayPort + a second sampled display). Do not fill
any cell from estimates.

## Purpose

Multi-display active sampling spawns one independent PipeWire stream per
selected display. Before multi-display sampling can be considered for anything
beyond opt-in, the marginal resource cost of a second concurrent stream must be
measured against the one-stream baseline. **If the two-stream resource cost is
materially non-zero, multi-display sampling stays opt-in.**

## One-stream baseline (already measured)

From `docs/research/2026-07-31-m2-capture-spike.md` (KDE Plasma Wayland,
`wayland-0`, AOC output DP-1, 30-minute windows unless noted):

| measurement | one-stream baseline |
|---|---:|
| warm-paused idle CPU delta vs closed — `kwin_wayland` | −0.089 percentage points |
| warm-paused idle CPU delta vs closed — `pipewire` | −0.008 percentage points |
| pause→resume→first-frame latency (30 samples, nearest-rank) | p95 14.377 ms (min 5.627 ms, max 172.557 ms) |
| one-frame acquisition via portal + GStreamer (upper bound, incl. setup/teardown) | 213.770 ms initial; 241.032 ms restore |
| sRGB→luma + 16×9 reduction, NumPy reference (not the Rust target) | median 173.431 ms; p95 180.329 ms |

## M1 — five-minute idle CPU, both displays sampling

Warm-paused streams on both selected displays, idle desktop, 300 s window.
Method: same `/proc/<pid>/stat` tick sampling as the baseline (`USER_HZ=100`),
stable process IDs recorded before the window.

| process | ticks start | ticks end | CPU % | delta vs closed | delta vs one-stream baseline |
|---|---:|---:|---:|---:|---:|
| `kwin_wayland` | TBD | TBD | TBD | TBD | TBD |
| `pipewire` | TBD | TBD | TBD | TBD | TBD |
| `dormantd` | TBD | TBD | TBD | TBD | TBD |

Comparison row against baseline: see the last column — anything materially
above the one-stream deltas (−0.089 / −0.008 pp) is a finding, not noise.

## M2 — capture-resume latency, both streams active

Pause→resume→first-frame per stream while the other stream is also held warm.
30 samples per stream, nearest-rank percentiles, same method as the baseline.

| stream | n | min | p50 | p95 | max |
|---|---:|---:|---:|---:|---:|
| display A (`TBD`) | TBD | TBD | TBD | TBD | TBD |
| display B (`TBD`) | TBD | TBD | TBD | TBD | TBD |
| one-stream baseline (p95) | 30 | 5.627 ms | — | 14.377 ms | 172.557 ms |

## M3 — capture cadence, ten ticks per stream

Ten consecutive capture ticks per stream with the daemon at its configured
`wear.sample_interval`. p50/p95 computed from the sampler's stage timestamps
(capture request → frame reduced), not wall-clock around the CLI.

| stream | tick | request→frame ms | reduce ms | stage-timestamp source |
|---|---|---:|---:|---|
| A | 1..10 | TBD | TBD | TBD |
| B | 1..10 | TBD | TBD | TBD |

| stream | n | p50 request→frame | p95 request→frame | p50 reduce | p95 reduce |
|---|---:|---:|---:|---:|---:|
| A | 10 | TBD | TBD | TBD | TBD |
| B | 10 | TBD | TBD | TBD | TBD |

## Decision rule

- Idle CPU and latency within noise of the one-stream baseline → multi-display
  sampling carries no measurable penalty; no default change implied.
- Materially non-zero resource cost (either process CPU delta, resume p95, or
  cadence p95 clearly above baseline) → multi-display sampling **stays
  opt-in**; record the measured cost here and in
  `docs/src/active-wear-sampling.md`.
