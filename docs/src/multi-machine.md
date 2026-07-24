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
