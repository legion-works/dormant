# macOS M0 spike — on-target verification (2026-07-16)

Live probes on the operator's MacBook Pro (M3 Pro, macOS 26.5) with the AOC
AG326UZD attached over USB-C as the external display. Complements the
2026-07-14 read-only recon (idle counter, gamma reads, IORegistry DDC surface).
Spike artifacts live on the Mac at `~/dormant-spike/` (gamma_spike.swift,
wake_spike.swift) — quarantined, not part of this repo.

Verdict: **GO for M1 on every probed mechanism**, with one major design
simplification — the gamma crash-blackout premise is falsified on this target.

## G1 — gamma-black works (operator-confirmed)

`CGSetDisplayTransferByTable` with an all-zero 1024-entry ramp on the external
display:

- Panel went visibly black (operator watched it), main built-in display
  unaffected.
- Readback during black: `first=0.0 last=0.0`; after in-process restore of the
  saved table: `last=1.0`.
- Restore path: writing back the saved per-channel table works (`RESTORED via
  saved table`).

## G2 — the crash-blackout premise is FALSE on macOS 26.5

The design council (REV-3/REV-4, 2026-07-12) built a four-layer recovery
mechanism on the premise that gamma LUT state persists after the setting
process dies. On-target result: **it does not.**

| Probe | Result |
|---|---|
| Set black, `exit(0)` without restore | table back to `last=1.0` within 4 s of process death |
| Set black, hold, `kill -9` mid-black | table back to `last=1.0` within 4 s of SIGKILL |

The WindowServer reverts a process's gamma modifications when that process's
connection dies — clean exit and SIGKILL behave identically. This matches the
historically documented CoreGraphics behavior (gamma changes are
process-scoped) rather than the BetterDisplay-derived persistence claim.

Implications for the spec (needs a REV-5 amendment before M1 build):

- Layers 2–3 of the recovery design (signal-path restore, startup breadcrumb
  restore) lose their motivating failure mode: a crashed daemon cannot strand
  a black panel. A breadcrumb is still cheap insurance for a *wedged-but-alive*
  daemon, but it is no longer load-bearing.
- Layer 4 (`dormantctl emergency-wake` calling
  `CGDisplayRestoreColorSyncSettings()`) stays — verified working from a fresh
  process, system-wide, no daemon required.
- Consequence to model instead: the daemon must HOLD its gamma black (the
  setting process must stay alive for the blank to persist) — a daemon restart
  mid-blank auto-wakes the panel. That is fail-safe in exactly the direction
  dormant wants (a screen that won't wake is the worst failure; a screen that
  wakes on daemon death is acceptable).

## DDC — m1ddc on ARM + USB-C: clean, fast, zero retries

The `ddc-macos` issue #8 defect (ARM+USB-C reads failing without retries) did
not manifest through m1ddc on this hardware:

- Luminance read ×5: `100` every time, 88–91 ms per call, no retry needed.
- Write path: `set luminance 40` → readback `40` → `set luminance 100` →
  readback `100`. Visible dip confirmed.
- Contrast read: `60`.

m1ddc's IOAVService approach (private surface) is the proven path; the M1 task
that patches/forks `ddc-macos` should mirror its transaction shape. Note
m1ddc does not expose arbitrary VCP opcodes — usage-hours (`0xC0`) seeding for
the wear ledger needs raw VCP support in our fork.

## Display sleep / wake — D1 mechanism verified end-to-end

- `pmset displaysleepnow` → both displays report `CGDisplayIsAsleep = true`
  within 4 s (readback primitive works).
- `IOPMAssertionDeclareUserActivity(kIOPMUserActiveLocal)` from a fresh
  process: `ret=0`, both displays awake 2.5 s later.
- The `caffeinate` assertion permanently running on this Mac did not block
  either direction (forced sleep and user-activity wake both punch through).

## Environment notes

- The external AOC enumerates as display id 2 (`CGDisplayIsBuiltin` filter is
  a sufficient selector on this setup); m1ddc lists it as `[2] AG326UZD`.
- Built-in display was main at spike time (arrangement differs from the
  2026-07-14 recon note — do not hard-code main-display assumptions).
- Gamma table capacity 1024 on both displays.
- `swiftc` single-file compiles work over SSH; fish login shell requires
  `ssh mac bash -s <<'EOF'` heredocs.
