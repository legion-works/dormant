# Multi-machine KVM switching

**What this gives you.** Two machines, one OLED: pull the panel to whichever
one you are using with a hotkey, the tray, the CLI, or the web — no pairing,
no network protocol. Each machine writes its own input code directly over its
DDC bus; ownership is observed from VCP `0x60` polling, not negotiated.

**When to use it.** You have one good panel connected to two machines (or one
machine and one console) and would rather not buy a KVM. Not for you if you
have a real KVM or a dedicated video matrix with stable EDID-controlled
switching, or if either machine lacks a DDC/CI-capable output.

**Quick setup.** Add `scope = "shared"`, `shared_input_code`, and
`shared_input_write_code` to both ends; add `shared_peer_input_code` for push
support. Verify each machine can read and write VCP `0x60`:

```bash
dormantctl doctor ddcci
```

---

One physical monitor can serve two `dormant` instances through a KVM or a
multi-input panel. Mark that display `shared` on both machines and give each
machine its own input-source code. Selection is a direct local DDC write —
there is no network protocol, no peer transport, no pairing, no crypto.

## Set up a shared display

1. Find the read-back code for each machine. Select the machine's input on the
   panel, then run:

   ```bash
   ddcutil --bus <N> getvcp 60
   dormantctl doctor ddcci
   ```

   Record the `0x60` value on that machine — this is the **READ** code, what the
   panel reports when this input is active. Switch the monitor to the other
   input and repeat there.

   **Next, verify the write code.** Most panels use the same value for both
   read and write, but some (e.g. certain AOC AGON models) accept a different
   code on `setvcp 60` than they report on `getvcp 60`. Test it:

   ```bash
   # Stop dormantd and any other ddcutil users first, then:
   ddcutil setvcp 60 0x0f --bus <N>   # try the read-back value
   ddcutil getvcp 60 --bus <N>          # did it switch?
   ```

   If the write fails silently (the panel stays on the old input), probe the
   codes advertised in `ddcutil capabilities --bus <N>` for the VCP 60
   feature — the panel may list an "Unrecognized value" that is actually the
   working write code. Try each candidate; confirm with a physical check.
   Record the working write value as `shared_input_write_code`.

2. Mark the same physical display shared in both configurations. Each machine
   uses its own code:

   ```toml
   [displays.shared_oled]
   controllers = ["ddcci"]
   blank_mode = "power_off"
   scope = "shared"
   shared_input_code = 0x0f       # what getvcp 60 reports for this input
   shared_input_write_code = 0x15 # what setvcp 60 needs (omit if same)
   ```

   If the peer machine sits on a different input, also configure its codes
   so push works:

   ```toml
   shared_peer_input_code = 0x10       # what getvcp 60 reports for the peer's input
   shared_peer_input_write_code = 0x15 # what setvcp 60 needs for the peer (omit if same)
   ```

## How switching works

There is no network pairing, no mDNS discovery, no cryptographic handshake,
and no cross-host protocol. Every switch is a **direct local DDC write** on the
acting machine's own bus — the machine writes an input code and the panel
moves.

### PULL — write my input code

`dormantctl switch <display>`, the tray hotkey, the tray "Switch to here"
menu item, the web UI "Switch to here" button, and a local activity edge
(when `activity_follow` is on) all **pull**: they write the acting machine's
own `shared_input_write_code` to the panel's VCP `0x60`.

Pulls are always safe: the acting machine is awake, its output is driving
signal, and the write lands.

```bash
# Pull the panel to this machine (write my input code)
dormantctl switch shared_oled
# Push the panel to the peer (write the peer's code; requires shared_peer_input_write_code)
dormantctl switch shared_oled --to-peer
```

### PUSH — write the peer's input code

`dormantctl switch <display> --to-peer` and the web UI "Send to peer" button
**push**: they write the peer's code. This path exists **only when
`shared_peer_input_write_code` is configured** on the display; without it the
CLI returns "not configured" and the web affordance is absent.

Push is deliberately minimal — no wake, no retry. If the peer's output is
not driving signal, the write is ACKed by the DDC bus but the panel silently
ignores it (see [Signal-presence law](#signal-presence-law)). Push is for the
common case: both machines are awake and the operator wants to switch away
without reaching the other keyboard.

### Semantic read/write aliases

`shared_input_code` is what the panel **reports** at VCP `0x60` when this
input is active; `shared_input_write_code` is what you **write** to select
it. On the maintainer's AOC AG326UZD these differ: write `0x15`, read back
`0x10`. A write is verified against the READ alias — a mismatch is a
`write_input_source` failure.

`shared_peer_input_code` and `shared_peer_input_write_code` are the peer-side
pair. When the peer READ alias is absent, the push verification degrades to
"changed away from my code" and the daemon logs `kvm_push_verification_degraded`.

## Ownership — an observation, not authority

Each machine reads its own VCP `0x60` every `poll_interval` (default `2s`).
Ownership is a **local observation** of what the panel reports — the daemon
never broadcasts "I own the panel" to a peer, and nothing consults ownership
before writing.

Ownership **gain** and **loss** are debounced differently depending on the path:

- **Gain observed by the poll** follows the same `loss_confirmations` (default
  `3`) consecutive agreeing reads as loss — a single "mine" reading does not
  immediately commit ownership. This defends against cross-machine DDC
  collisions returning the same wrong code N times (issue #134).
- **Gain from a verified local pull** (`dormantctl switch`, tray, web, hotkey,
  activity follow) calls `mark_owned_immediate` after the write-verification
  readback confirms the panel moved — the machine has first-hand proof of
  ownership, so the poll's debounce is bypassed. There is no ~7.5 s wake lag on
  the acquiring side (`coordination.rs:305`, `direct_switch.rs:258`).
- **Loss** is always debounced through `loss_confirmations` consecutive "not
  mine" reads. With defaults a loss takes ~6 seconds to commit (`2s` × 3).
  During that window the old owner still believes it owns the panel and may
  issue a blank that can land on top of the new owner's wake. Lower
  `loss_confirmations` toward `1` to shrink the window at the cost of
  flap-susceptibility.

### Flap containment

Consecutive-read debounce cannot reject an input-source flap that holds each
side longer than its confirmation window. The daemon therefore tracks committed
ownership transitions in `flap_window` (default `120s`). At
`flap_threshold = 8`, it marks the display **contested** and forces the local
verdict to not-owned until `flap_settle = 60s` passes without another committed
transition. Eight transitions tolerate four human KVM round-trips in two
minutes; a 6–10 second standby flap produces 12–20 and is contained in the
first 90 seconds. The 23 yields observed over an afternoon of ordinary KVM use
are spread across hours and do not trip the burst threshold.

Contested never freezes an owned verdict. If the peer has the panel, retaining
owned could let this daemon advance to a DDC power-off stage on the peer's
screen. Treating the input as unknown and not-owned obeys the fail-safe rule:
the local source may not blank a panel it cannot identify as its own. A verified
local input write clears contested immediately because its readback is
first-hand proof.

A read whose observed code is **neither** the local nor the peer input code is
treated as a transport failure — the prior verdict is held and the debounce
counter is not incremented. Only observations that classify as `Local`,
`Peer`, or a definite debounce-influencing pattern reach the state machine
(`coordination_poll.rs:124-137`, issue #138 part B). A read failure also
holds the prior verdict.

### Convergence, not mutual exclusion

There is no cross-host lock. Two genuinely simultaneous activity edges on
different machines both write once and the panel settles to whichever write
arrived last; both fire acquire hooks, and the loser self-corrects on its
next poll. Guaranteed by design and test-proven: at most one write per
admitted local edge, zero poll-caused writes, no sustained ping-pong. The
rare double-flip is the accepted cost of a lock-free design — do not expect
serialization we don't provide.

## Not the TV wear-sampling source gate

Multi-machine switching and Samsung TV source gating both read a "source",
but they are different mechanisms solving different problems:

- **This chapter** reads VCP `0x60` over DDC/CI to observe *which machine owns*
a shared panel, and writes the input code to pull the panel between machines.
It is a soft-KVM ownership switch.
- **[Active wear sampling](./active-wear-sampling.md)** reads Samsung IP
  Control (`inputSourceControl`) to gate *whether the TV is showing the HDMI
  input the sampler expects* before counting a captured frame as wear. It
  never switches the TV's input and never writes anything; it is wear-
  attribution safety so a TV on Netflix does not accrue local-frame wear.

The two never interact: the DDC ownership poll is local to each machine's own
bus, and the wear-sampling source gate is a read-only check against the TV's
reported input. A `samsung-tizen` display carrying `[displays.<id>.sampling]`
gets the wear gate regardless of whether any display is `scope = "shared"`.

## Activity follow

When `coordination.activity_follow = true` (default `false`), a **genuine
local activity edge** — keyboard, mouse, or tablet input — pulls the shared
display to this machine after `arm_after` idle (default `7s`). The jiggler
and ignored-device filter mean an ignored device produces no edge.

`coordination.cooldown` (default `3s`) suppresses **only** activity-driven
pulls. Hotkeys, CLI switches, tray actions, and web buttons deliberately
bypass cooldown — an explicit operator action is never swallowed.

Activity follow has a warm-up: a pull only fires if the input source was not
already local (the display is either showing the peer or an unknown source).

## Hooks

Each shared display can declare action slots. Actions in a slot run in
declaration order. A hook action is either a command (argv array, no shell)
or an MQTT publish (QoS 1, non-retained).

### Hook slots and their timing

| Slot | Direction | When it fires |
|---|---|---|
| `before_acquire` | Pull (acquire) | BEFORE the local DDC write. Blocking — a failure aborts the pull. This is the initiator's wake-and-verify slot. |
| `after_acquire` | Pull (acquire) | AFTER a successful pull. Fire-and-forget — the pull already completed. |
| `before_release` | Push (release) | BEFORE the peer DDC write. Blocking — a failure aborts the push. Use for USB-switch or KVMP transitions. |
| `after_release` | Push (release) | AFTER a successful or failed push write. Fire-and-forget. |
| `on_observed_loss` | Poll (loss) | AFTER the poller commits an ownership loss. Fire-and-forget — the poll path has no write authority and must never trigger a corrective DDC write or retry. |

### Hook causality — who gets advance notice

The **initiating** machine gets real `before_acquire` / `after_acquire`
timing — blocking, local, before its own write.

The machine that **loses** the panel to a peer's pull gets no advance
notice — it learns from its own poll afterward and fires the
`on_observed_loss` after-only slot. `before_release` never fires post-hoc.

### Hook environment

Hook commands run in a deliberately minimal environment — the daemon's
build environment and unrelated secrets must not leak into a hook child.

Every hook command receives:

| Variable | Source | Meaning |
|---|---|---|
| `PATH` | hard-coded | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` — the daemon's PATH is never inherited |
| `HOME` | daemon | Inherited so `~` resolution works |
| `WAYLAND_DISPLAY` | daemon (if set) | Wayland compositor socket |
| `XDG_RUNTIME_DIR` | daemon (if set) | Per-user runtime directory |
| `DISPLAY` | daemon (if set) | X11 display (XWayland) |
| `XDG_SESSION_TYPE` | daemon (if set) | `wayland` or `x11` |
| `DBUS_SESSION_BUS_ADDRESS` | daemon (if set) | User D-Bus session bus |
| `DORMANT_DISPLAY` | context | Config display id |
| `DORMANT_DISPLAY_IDENTITY` | context | Claim identity (`manufacturer:model[:serial]`) |
| `DORMANT_DIRECTION` | context | `acquire` or `release` or `observed_loss` |
| `DORMANT_PHASE` | context | `before` or `after` |
| `DORMANT_PEER` | context | Peer's instance id (not display name) |
| `DORMANT_FALLBACK` | context | `0` or `1` — whether this slot is firing on the fallback path |
| `DORMANT_ABORTED` | context | `0` or `1` — whether this is a write-failure compensation channel |

Nothing else is inherited. If the daemon does not have a session var (e.g.
`WAYLAND_DISPLAY` on a non-Wayland session), it is simply absent from the
child — no empty-string injection.

**A `blocking = true` `before_acquire` hook is an abort gate.** If the hook
fails for any reason (non-zero exit, timeout, spawn failure, missing binary),
the switch is cancelled and `claim_acquire_aborted` is logged. An
environmentally-broken hook is therefore a correctness risk — verify your
hook commands run successfully under the hook environment before deploying.
See [Signal-presence law](#signal-presence-law) for why the
`before_acquire` wake-and-verify slot is load-bearing.

### Scheduling and idempotence

The `blocking` default is phase-dependent: `before_*` slots default to
blocking, `after_*` slots default to fire-and-forget
(`hooks.rs::default_blocking_for`). `on_observed_loss` is an `after_*` slot
and therefore defaults to non-blocking. Entries declared `blocking = true` run
to completion and block the next phase; non-blocking entries are spawned and
the phase continues immediately. Hooks are bounded by their per-entry `timeout` (default `5s`),
not cancellable mid-run. Hook commands must be idempotent — check
`DORMANT_DIRECTION` and `DORMANT_PHASE` in the environment to decide whether
to act or skip.

## macOS DDC/CI power-off hazard

A `power_off` blank on a shared macOS DDC/CI panel can be unrecoverable:
the USB-C link and the panel's USB hub drop on standby, VCP writes go
to a dead device, and recovery requires physically power-cycling the
monitor. The full mechanism, the observed behavior on the maintainer's
AOC AGON AG326UZD, and the explicit acknowledgement escape hatch
(`displays.<id>.power_off_opt_in = true`) live in
[Displays → macOS shared-DDC/CI power-off hazard](./displays.md#macos-shared-ddcci-power-off-hazard).
**Read that section before using `controllers = ["ddcci"]` with
`blank_mode = "power_off"` on a shared display from a macOS host.**
Dormant emits a load-time semantic warning and a `dormantctl doctor`
failure for every hazardous display — the opt-in silences both, it does
not add recovery.

## Signal-presence law

A monitor **will not switch VCP `0x60` to an input that has no live video
signal.** This was verified on the AOC AGON AG326UZD with both a Linux
desktop (DisplayPort) and a macOS machine (DisplayPort). The panel ACKed
`setvcp 60` writes to a dark input, reported them as successful, but
readback confirmed VCP `0x60` stayed on the active input.

This is why push fails safe and why the daemon does not try to wake a peer:
a machine whose output is dark cannot receive a panel pull. The initiating
machine's own `before_acquire` hooks (wake the output, verify signal) are the
only path to a working switch.

## Linux permissions

The activity-follow path reads keyboard and mouse events. On Linux, the
default input source is the Wayland compositor's idle-notifier protocol or
the D-Bus screensaver idle time — neither requires elevated permissions.

When `[input_filter] ignore_devices` is configured, the daemon opens evdev
`/dev/input/event*` nodes through the compositor's seat to filter out
named devices. The opener needs read access to those nodes, which typically
means the user running `dormantd` must be in the `input` group, or the
system's `uaccess` / logind ACL must grant the active seat access.

Verify the backend with:

```bash
dormantctl doctor input-filter
```

The probe confirms that `/dev/input/event*` nodes are readable and reports
which devices match the configured `ignore_devices` globs. If the probe fails
with "permission denied", add the user to the `input` group and re-login.

## macOS Accessibility

On macOS, the tray hotkey (Carbon `RegisterEventHotKey`) requests **no**
Accessibility permissions. The macOS `CGEvent` tap (the input-filter backend
for activity follow on macOS) **does** require Accessibility permissions.

## InputWake validation

When an activity-driven pull fires and the render sink is the active stage,
the daemon waits for the first real input event from the new owner — the
**InputWake**. This proves the input source actually reached the panel and
that the display is now showing the active framebuffer.

The validator is a 500 ms bounded window: if a valid edge arrives, the pull
completes immediately; if the window expires, the pull still completes — the
validator is a best-effort proof, not a gate.

## `[coordination]` reference

Coordination is opt-in — it activates only when at least one display has
`scope = "shared"`. There is no `enabled` key; the section's presence (or
any key within it) has no effect without a shared display.

| Key | Type | Default | Description |
|---|---|---|---|
| `poll_interval` | duration | `"2s"` | Shared-display ownership poll cadence (VCP `0x60`); minimum `"1s"`. |
| `state_poll_interval` | duration | unset | Panel-state (brightness/power) refresh cadence for `DisplaySnapshot` cosmetics. When unset, defaults to `max(30s, poll_interval)`; when set, must be `>= poll_interval`. Ownership still polls at `poll_interval`; only panel state refreshes here, to cut per-transaction i2c traffic. |
| `loss_confirmations` | integer | `3` | Consecutive agreeing VCP `0x60` readings required before the cached ownership verdict flips — symmetric for gain and loss. Defends against garbled reads from concurrent cross-machine DDC traffic (issue #134). Validated `1..=10`. A verified local pull marks ownership immediately (the machine has first-hand proof), so `loss_confirmations` only governs the poll-observed transitions. |
| `flap_threshold` | integer | `8` | Committed transitions permitted within `flap_window` before ownership is contested; minimum `2`. Eight permits four normal KVM round-trips in two minutes but catches the 12–20 transitions from a 6–10s standby flap. |
| `flap_window` | duration | `"120s"` | Sliding transition-count interval; must be `>= poll_interval`. |
| `flap_settle` | duration | `"60s"` | Quiet interval before a contested display resumes normal evaluation; must be `>= poll_interval`. Contested forces not-owned so this machine cannot power off a panel the peer may be using. |
| `activity_follow` | boolean | `false` | When `true`, a genuine local activity edge (keyboard, mouse, tablet) pulls a shared display to this machine after `arm_after` idle. |
| `arm_after` | duration | `"7s"` | Grace window after receiving a local arm before the pull commits (only meaningful when `activity_follow = true`). |
| `cooldown` | duration | `"3s"` | Minimum interval between successive activity-driven pulls. Hotkeys, CLI, tray, and web bypass this — an explicit operator action is never swallowed. |

## Example: shared display with hooks

```toml
[displays.shared_oled]
controllers = ["ddcci"]
blank_mode = "power_off"
scope = "shared"
shared_input_code = 0x0f
shared_input_write_code = 0x15
shared_peer_input_code = 0x10
shared_peer_input_write_code = 0x15

[displays.shared_oled.hooks]
before_acquire = [
  # Wake the machine's video output so the panel will accept the switch.
  { command = ["xset", "dpms", "force", "on"], timeout = "5s", blocking = true },
]
after_acquire = [
  { command = ["notify-send", "panel acquired"], timeout = "5s", blocking = false },
]
before_release = [
  # Switch the USB peripheral hub to the peer before releasing the panel.
  { mqtt = { topic = "usbswitch/set", payload = "mac" }, timeout = "5s", blocking = true },
]
on_observed_loss = [
  # The poller detected a peer pull — this machine lost the panel.
  { command = ["notify-send", "panel released to peer"], timeout = "5s", blocking = false },
]
```

## Limits and failure behavior

- VCP `0x60` reports the main window only. PIP/PBP can show another source
  while readback still names the main input; do not use shared coordination to
  automate a PIP layout.
- If a third monitor input is selected, both configured machines are non-owners
  until one of their configured inputs returns.
- A DDC read error holds the last ownership verdict. At cold start or after a
  stale observation, ownership stays conservative. An unknown zone acquiring
  ownership does not wake the panel.
- Two daemons polling the **same** physical panel will see occasional
  successful-but-wrong reads on each other's bus traffic. The
  `loss_confirmations` debounce holds the prior verdict on a stray "not mine"
  reading; `coord_poll_disagreement` (when consecutive observations disagree),
  `coord_ownership_loss_deferred` (when the pending loss counter is below the
  threshold), and `coord_ownership_gain_deferred` (when the pending gain
  counter is in flight — the symmetric counterpart to the loss-deferred signal)
  are emitted as literal anchors so the operator can see the bus is
  dirty without parsing the verdict cache.
- The debounce reduces but does not eliminate false losses. If cross-machine
  DDC collisions return the same wrong code N times in a row, a false loss can
  still commit — N=3 makes a false loss ~3× less likely than N=1, not
  impossible. The `coord_poll_disagreement` signal only fires when consecutive
  observations *differ*; identical wrong readings sail through the debounce.
- `dormantctl doctor` can report `input_source=skipped` when a controller has
  no usable input-source readback, or `input_source=unreadable` when a read
  was attempted and failed. Fix that before relying on a shared display.

### Web UI network surface

The only remote control surface is the **web UI**. Under default configuration
the web server binds `127.0.0.1` only. Binding a non-loopback address is
rejected at startup unless `daemon.web_allow_nonloopback` is explicitly set
to `true`.

**If `web_allow_nonloopback = true` is set**, the following unauthenticated
endpoints become reachable from the LAN:

- `POST /api/switch` — switch display input
- `POST /api/push` — push the panel to the peer (write the peer input code; requires `shared_peer_input_write_code`)
- `POST /api/blank` — blank the panel
- `POST /api/wake` — wake the panel
- `POST /api/pause` / `POST /api/resume` — pause or resume rule processing
- `POST /api/emergency-wake` — immediate global wake
- `POST /api/reload` — reload config from disk
- `POST /api/config/apply` — write a new config
- `POST /api/doctor` / `POST /api/doctor/exercise/:display` — run probes
- `POST /api/pair/samsung` — pair with a Samsung TV

The blank, switch, pause, and resume routes fire operator-configured hooks
that run arbitrary commands (including MQTT publishes) — a wider surface than
"someone can flip my monitor input." The web UI has no authentication layer
of its own; it relies entirely on the loopback bind for access control.
Opening the bind to the LAN removes the only barrier.

## Operator migration — `coordination.enabled` removed

The `coordination.enabled` key was removed from the config schema. Prior to
the direct-write pivot this key controlled mDNS discovery, instance pairing,
and the claim protocol — all now deleted. Existing configs that set
`enabled` will fail strict unknown-key validation until the key is deleted
from the TOML file.

```bash
# To fix: remove the stale key from your config.
# Run validate to confirm:
dormantctl validate
```

If your config also carried any of the other removed coordination keys,
delete those too — they no longer exist in the schema:

```toml
# All of these are gone. Delete any that appear under [coordination]:
#   enabled                 pairing_port            pairing_window
#   pairing_bind_address    claim_port              claim_bind_address
#   claim_advertise_mdns    activity_claim          owner_idle_window
#   armed_window            claim_timeout           release_deadline_cap
```

The six keys that survive are `poll_interval`, `state_poll_interval`,
`loss_confirmations`, `activity_follow`, `arm_after`, and `cooldown`.
