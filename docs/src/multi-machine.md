# Multi-machine coordination

One physical monitor can serve two `dormant` instances through a KVM or a
multi-input panel. Mark that display `shared` on both machines and give each
machine its own input-source code. Only the machine selected on the monitor
controls the panel; the other instance leaves it alone.

`dormant` does not broadcast panel ownership. Each machine reads DDC/CI VCP
`0x60` from the monitor it controls and treats that local readback as truth.
mDNS and pairing identify nearby instances; they do not carry presence, panel
state, or a liveness heartbeat. MQTT is not required.

## Set up a shared display

1. Enable coordination on both machines, then restart each daemon:

   ```toml
   [coordination]
   enabled = true
   ```

2. Find the input code for each machine. Select the machine's input on the
   panel, then run:

   ```bash
   ddcutil --bus <N> getvcp 60
   dormantctl doctor
   ```

   Record the `0x60` value on that machine. Switch the monitor to the other
   input and repeat there. Input codes are deployment-specific; do not infer
   them from `ddcutil capabilities`, whose reported values are unreliable for
   this purpose.

3. Mark the same physical display shared in both configurations. Each machine
   uses its own code:

   ```toml
   [displays.shared_oled]
   controllers = ["ddcci"]
   blank_mode = "power_off"
   scope = "shared"
   shared_input_code = 0x0f # replace with this machine's recorded code
   ```

   A newly shared display starts with conservative ownership after reload. It
   must receive a local input-source observation before normal coordination
   resumes.

4. Pair the instances. Open an instance pairing window on one machine and copy
   its one-time code to the other. The loopback API exposes
   `POST /api/pair/instance`, `GET /api/pair/instance/{id}`, cancellation at
   `POST /api/pair/instance/{id}/cancel`, discovery at
   `GET /api/pair/instance/peers`, and join at
   `POST /api/pair/instance/join`.

   The web UI's `DormantPairing.tsx` component opens the pairing window, shows
   the one-time code and expiry, and polls pairing status.

   The CLI has the same responder/initiator flow:

   ```bash
   # On the responder: opens a local window and prints the one-time code.
   dormantctl pair instance "Office Mac" --open

   # On the initiator: joins a discovered peer.
   dormantctl pair instance "Office Mac" --code ABCD1234
   ```

   If discovery finds multiple peers with the same name, rerun the join command
   with `--instance-id <id>`. A peer must be discovered before it can be joined.

## Ownership and operator state

The display snapshot carries `scope`, `owned`, `observed_input_code`, and
`panel_state`. These fields distinguish a locally owned panel from a deferred
one and report the input/panel observation that produced the verdict.

Force actions remain monitor-global. The tray calls this out as **Blank shared
panel — affects all connected machines**. Use Force wake for immediate recovery
when a panel is dark; force blank bypasses normal presence rules and affects
whichever source is selected.

### Claim methods

Shared-display claims can be initiated in four ways:

- **Hotkey** — immediate claim (factory-mapped to `Meta+Ctrl+Shift+B`).
- **`dormantctl switch <display>`** — immediate claim from the CLI.
- **`activity_claim = "edge"`** — claims on a local input edge (keyboard,
  mouse, tablet).
- **`activity_claim = "owner-idle"`** — claims once the current owner (the peer
  selected on the monitor) has been idle for at least `owner_idle_window`.

`owner-idle` has a warm-up requirement: it needs one prior successful claim
per display before it engages. The daemon learns the owner's identity from
`ClaimResponse::Accepted` — not from the config, because ownership is local
hardware truth and is never broadcast between peers. Until the first claim
completes (via hotkey, `dormantctl switch`, or `activity_claim = "edge"`),
`owner-idle` will not fire. This is a one-time per-display requirement; after
the daemon restarts, the owner identity must be relearned.

See [`activity_claim`](#coordination-reference) and
[`Limits and failure behavior`](#limits-and-failure-behavior) for details.

## Pairing security

Opening a pairing window makes one machine the responder. It displays an
eight-character, one-time Crockford Base32 code, advertises over mDNS, and
listens only during the configured window. The initiator discovers that
advertisement and supplies the code.

The code is the password for SPAKE2, not a comparison string after an
unauthenticated exchange. Both machines also have persistent Ed25519
identities. After SPAKE2, they exchange public identities and verify the full
transcript with HMAC-SHA256. A wrong code, active man-in-the-middle, identity
substitution, or protocol downgrade fails confirmation and writes no peer.

The private identity and `peers.json` are stored with `0600` permissions. The
code is shown once to the local opener and is never written to the peer store,
status responses, or logs.

The listener accepts at most ten attempts, expires with the code, and closes on
success or cancellation. There is no always-on pairing port. During a live
window, an attacker on the LAN can consume the attempt budget or flood/drop
traffic. That denial of service is accepted: retrying opens a new short window,
and no peer is persisted without completed confirmation.

## Claim protocol

Two paired `dormant` daemons negotiate panel ownership in one round trip,
with optional fallback for unresponsive peers.

### Identity and probe

Every shared display pair requires a **claim identity**: a canonical
`manufacturer:model` string (plus `:serial` when the EDID reports one)
derived from the panel's EDID text fields. Both machines must agree on this
identity — a serial mismatch or a missing required field blocks the claim.

Confirm the identity on each machine:

```bash
dormantctl doctor ddcci
```

Look for `claim_identity=` in the probe output. If the string differs
between the two hosts, check the physical display connection: a display
connected through a different port or a different EDID path on one side
will produce a different claim identity.

The identity uses the EDID text fields only, **never** the machine-local
`ident_string` bus prefix — machines connected through different DDC buses
(e.g. `i2c-dev:7` vs `i2c-dev:8`) will still produce the same claim identity
as long as the panel's EDID manufacturer, model, and serial are identical.

### Paired negotiated path

A hotkey press or `dormantctl switch` fires an immediate claim. The requester
broadcasts a signed `ClaimRequest` frame to every paired peer on the LAN over
TCP (the port announced via the `_dormant-claim._tcp.local.` mDNS service or
reached through a previously dialled address). Each peer validates the frame,
runs the owner-side state machine (hooks + input-source write), and returns a
signed `ClaimResponse`. The protocol enforces replay protection per peer
(monotonic outbound counters) and per-peer epoch validation (stale-epoch
responses from a restarted peer are rejected).

The claim succeeds when one peer accepts and the requester reads its own VCP
`0x60` code on the panel within the negotiated `release_deadline_cap`. A
`Denied`, `NotOwner`, or `Busy` response from every expected peer triggers the
fallback path.

### Powered-only direct fallback

When every expected peer responds `NotOwner` (no peer claims to own the
display), or when the `claim_timeout` expires without any `Accepted`, the
requester falls back to a direct VCP `0x60` write. This path skips the peer's
`before_release` hooks — the requester writes its input code directly to the
panel, waking it if it was blanked.

The direct fallback is recorded as `claim_fallback_direct`. It is the only
available path when the panel is powered but no paired machine has `dormantd`
running (e.g. a single-machine setup with a monitor that was previously
sleeping, or a third-party device selected on the panel).

### Visible-standby failure

If the panel is in a low-power standby state where DDC/CI VCP writes fail
(or where the VCP `0x60` readback is not reliable), the claim negotiation
still proceeds — the `OwnerDisposition::Ready { standby: true }` flag tells
the owner its `before_release` hooks and the subsequent VCP write may fail.
The requester falls back to the direct path; the observable effect is a
visible power-state transition on wake that did not complete in the negotiated
phase.

### Before-release fallback limitation (load-bearing)

The `before_release` hook slot is the **only** mechanism that can sequence a
USB-switch, KVMP, or other external transition ahead of the DDC input-source
write. If the owner's `before_release` hooks fail and the owner sends
`ReleaseFailed`, the peer's `after_release` hooks receive the abort
compensation via `DORMANT_ABORTED=1` (see [Hook environment](#hook-environment)).
There is no retry loop within a single flight — a failed owner transition
terminates the flight, and the requester must retry from the beginning.

## Hooks

Each shared display can declare action slots for the four hand-off phases:
`before_release`, `after_release`, `before_acquire`, and `after_acquire`.
Actions in a slot run in declaration order. A hook action is either a command
(argv array, no shell) or an MQTT publish (QoS 1, non-retained, separate
client from the sensor-plane MQTT).

### Scheduling and idempotence

Hooks are NOT cancellable mid-run — they are bounded by their per-entry
`timeout` instead. Entries declared as `blocking = true` (the slot default)
run to completion and block the next phase; non-blocking entries are spawned
and the phase continues immediately. A hook that fires after a late arrival
(e.g. the direct-fallback path) is documented as **at-least-once**: hook
commands MUST be idempotent. Check `DORMANT_DIRECTION`, `DORMANT_PHASE`, and
`DORMANT_FALLBACK` in the environment to decide whether to act or skip.

### Hook environment

Every hook command receives:

| Variable | Meaning |
|---|---|
| `DORMANT_DISPLAY` | Config display id |
| `DORMANT_DISPLAY_IDENTITY` | Claim identity (F5, `manufacturer:model[:serial]`) |
| `DORMANT_DIRECTION` | `release` or `acquire` |
| `DORMANT_PHASE` | `before` or `after` |
| `DORMANT_PEER` | Peer's instance id (not display name) |
| `DORMANT_FALLBACK` | `0` (negotiated) or `1` (direct fallback) |
| `DORMANT_ABORTED` | `0` (normal) or `1` (write-failure compensation; see below) |

### Write-failure compensation

When the owner's input-source write (`WriteSucceeded` / `WriteFailed`) fails,
the owner's `after_release` slot still executes — but with `DORMANT_ABORTED=1`.
This is the compensation channel for the foreign-owner case (spec §4 step 2):
the requester that initiated the claim didn't get the panel, and it can use
this signal to revert a USB-switch or KVMP transition that it performed in
`before_release`.

`DORMANT_ABORTED=1` is set for `after_release` hooks only, and only when the
write to the panel actually failed. `after_acquire` hooks on the requester
side never see `DORMANT_ABORTED=1` — if the claim completed, the panel was
acquired successfully.

### mDNS and claim port

Paired peers announce their always-on claim listener through
`_dormant-claim._tcp.local.` mDNS. The TXT record is deliberately minimal —
only `v` (protocol version), `instance_id`, and `port` — no display names,
counts, or hostnames are broadcast. Set `coordination.claim_advertise_mdns =
false` to stop advertising the listener while still accepting inbound
connections (requiring the peer to reach the machine through a previously
dialled or manually configured address).

The claim listener binds to a **fixed** port (`coordination.claim_port`) or an
OS-assigned ephemeral port (`0`). When `claim_advertise_mdns` is true, the
advertised port is the actual bound port; a changing OS-assigned port across
restarts is broadcast automatically.

## Activity policies

Three activity-claim policies (`coordination.activity_claim`) fire claims
automatically from local input, without operator action:

| Policy | Behavior |
|---|---|
| `off` | No automatic claims (default). |
| `edge` | Claim on any local input edge — keyboard, mouse, or tablet. The edge fires exactly once per flight; subsequent input during the same flight is ignored. |
| `owner-idle` | Claim when the panel's current owner (the peer selected on the monitor) has been idle ≥ `owner_idle_window`. Requires a prior successful claim per display to learn the owner's identity — this warm-up runs once per daemon lifetime. |
| `armed` | Claim while an explicit local arm window is active (opened by `dormantctl switch <display> --arm`). The arm expires after `armed_window` and must be re-armed. |

`owner-idle` idle reports are authenticated by the peer: the local daemon only
accepts `IdleReport` frames from the peer it learned from the most recent
`ClaimResponse::Accepted`. Until that first claim completes, every
`owner-idle` IdleReport is dropped — the daemon will not act on reports from
an unknown peer.

## Linux permissions

The activity-claim path reads keyboard and mouse events from the compositor's
input seat (evdev nodes on Linux, `CGEvent` tap on macOS). On Linux, the
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
Accessibility permissions — `RegisterEventHotKey` is part of the Carbon Event
Manager and registers directly with the HID system, bypassing the
`AXIsProcessTrusted` gate. The Carbon path sets a system-wide hotkey that
`dormant-tray` processes in its run loop; it does not observe or filter other
applications' events.

The macOS `CGEvent` tap (the input-filter backend for activity claims on
macOS) **does** require Accessibility permissions. Without it, the daemon
cannot read keyboard/mouse events from devices that are not already granted.
The `input_filter_active` / `input_filter_unavailable` log anchors report the
tap state — `input_filter_unavailable` means the tap could not be created and
activity claims relying on filtered input (e.g. `activity_claim = "edge"`)
will not fire. The stock idle source still reports activity through
CoreGraphics idle-time queries, so the `user-activity` inhibitor is
unaffected.

## InputWake validation (F8)

When `activity_claim = "edge"` or `"armed"` fires a claim and the render sink
is the active stage, the daemon waits for the first real input event from the
new owner — the **InputWake** (F8). This proves the input source actually
reached the panel and that the display is now showing the active framebuffer.

The validator is a 500 ms bounded window:

1. After the claim's input-source write succeeds, the daemon begins watching
   for a filtered-activity edge whose sequence number is **after** the start
   of the current claim flight.
2. Input events from ignored devices (matching `ignore_devices` globs) are
   silently discarded and do not satisfy the validator.
3. If a valid edge arrives within 500 ms, the claim completes immediately —
   the panel was acquired and the input source is confirmed.
4. If the 500 ms window expires or the activity source becomes unavailable,
   the claim still completes — the validator is a best-effort proof, not a
   gate. InputWake failure does not roll back the claim.

During the 500 ms window the render overlay may flicker briefly: the daemon
submitted the blank frame before the claim, the claim's write switched the
input, and the new owner's first composited frame arrives asynchronously.
This is one composited frame of black (typically <33 ms at 30 Hz), not a
multi-second stuck state.

## `[coordination]` reference

Coordination is opt-in. `enabled = false` disables mDNS, pairing, and the
instance-pairing routes, but it never disables local `0x60` ownership polling
for configured shared displays.

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `false` | Enables mDNS discovery, pairing, and instance-pairing routes. |
| `poll_interval` | duration | `"2s"` | Shared-display ownership poll cadence (VCP `0x60`); minimum `"1s"`. |
| `state_poll_interval` | duration | `"30s"` | Panel-state (brightness/power) refresh cadence for `DisplaySnapshot` cosmetics. When unset, defaults to `max(30s, poll_interval)`; when set, must be `>= poll_interval`. Ownership still polls at `poll_interval`; only panel state refreshes here, to cut per-transaction i2c traffic. |
| `loss_confirmations` | integer | `3` | Consecutive agreeing "not mine" VCP `0x60` readings required before the cached ownership verdict flips `true → false` (defends against garbled reads from concurrent cross-machine DDC traffic; issue #134). Validated `1..=10`. Ownership *gain* (`false → true`) stays eager — waking on a possibly-wrong "I own" read is idempotent and the next poll re-confirms. |
| `pairing_port` | integer | `0` | TCP port for a pairing window; `0` requests an ephemeral OS port. |
| `pairing_window` | duration | `"5m"` | Lifetime of the listener and mDNS advertisement; `"30s"` to `"15m"`. |
| `pairing_bind_address` | string or unset | unset | LAN address for the temporary listener; unset auto-detects the primary non-loopback address. |
| `activity_claim` | enum | `"off"` | Claim policy triggered by local input activity. `"off"` — no automatic claims. `"owner-idle"` — claim when the current owner is idle (requires one prior claim per display; see [Claim methods](#claim-methods)). `"edge"` — claim on a local input edge (keyboard/mouse/tablet). `"armed"` — claim while an explicit arm window is active. |
| `owner_idle_window` | duration | `"2m"` | Minimum owner-idle duration before an `owner-idle` claim fires. See [`activity_claim`](#coordination-reference) for the warm-up requirement. |

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
  reading; `coord_poll_disagreement` (when consecutive observations disagree)
  and `coord_ownership_loss_deferred` (when the pending counter is below the
  threshold) are emitted as literal anchors so the operator can see the bus is
  dirty without parsing the verdict cache.

### Ownership-loss debounce — latency and honest limits

`coordination.loss_confirmations` (default `3`) debounces ownership loss:
the verdict only flips `true → false` after N consecutive agreeing "not mine"
VCP `0x60` readings. With the defaults (`poll_interval = 2s` × N = 3), a
genuine input switch takes ~6 seconds to commit. During that window the
old owner still believes it owns the panel and can still issue a blank. The
new owner reads "mine" on its next poll, commits the gain eagerly, and
wakes the panel immediately — the panel is on, but the old owner's blank
can still land on top of the new owner's wake, producing a short visible
flicker if presence/absence transitions happen to align. Operators who
cannot tolerate that window can lower `loss_confirmations` toward `1`
(one-tick commit) at the cost of flap-susceptibility, or raise
`poll_interval` (less responsive in both directions).

The debounce reduces but does not eliminate false losses. Two daemons on one
DDC bus can collide in ways that return the same wrong code N times in a
row — N=3 makes a false loss ~3× less likely than N=1, not impossible. The
`coord_poll_disagreement` signal only fires when consecutive observations
*differ*; identical-wrong readings sail through the debounce as if they
were genuine. This is a fundamental limitation of cross-machine arbitration
on a single physical DDC bus: there is no out-of-band channel the daemon can
use to distinguish "the input really switched" from "the bus returned the
same wrong code N times". The companion defenses — hold-last-verdict on DDC
errors, eager wake on ownership gain, and the per-process `PanelLocks`
+ `DDC_PHYSICAL_GATE` (issue #127) — narrow the window but do not change
that limit. If a deployment sees false losses under load, the practical
mitigations are: increase `loss_confirmations` (widens the genuine-handoff
latency proportionally), increase `poll_interval` (less responsive in both
directions), or disable coordination on one of the machines
(`coordination.enabled = false`).
- `dormantctl doctor` can report `input_source=skipped` when a controller has no
  usable input-source readback, or `input_source=unreadable` when a read was
  attempted and failed. Fix that before relying on a shared display.
- Discovery is not a heartbeat. Losing an mDNS peer does not change local panel
  ownership, and local presence state is never shared between instances.
- **`owner-idle` warm-up.** The `owner-idle` activity-claim policy needs one
  prior successful claim per display before it engages. The daemon learns the
  owner from the first `ClaimResponse::Accepted`, which only arrives after a
  claim initiated via hotkey, `dormantctl switch`, or `activity_claim = "edge"`.
  Until that claim completes, `owner-idle` IdleReports are dropped (security:
  the daemon must not accept reports from an unknown peer). After a daemon
  restart, the owner identity must be relearned — `owner_instance_id` lives
  in memory, not on disk.

## Troubleshooting

**Peer is not discovered.** Confirm `coordination.enabled = true` on both
machines, restart the daemons, and open a pairing window on the responder.
mDNS advertisements exist only while that window is open. Check that both
machines can use the same LAN and that the temporary listener bind address is
reachable.

**The code expired.** Open another responder window. Codes are one-time and
valid only for `pairing_window`.

**Attempt limit reached.** A window accepts ten `PairHello` attempts. Cancel or
wait for it to expire, then open a new window. This can be caused by a wrong
code, duplicate retries, or LAN traffic during the active window.
