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
| `pairing_port` | integer | `0` | TCP port for a pairing window; `0` requests an ephemeral OS port. |
| `pairing_window` | duration | `"5m"` | Lifetime of the listener and mDNS advertisement; `"30s"` to `"15m"`. |
| `pairing_bind_address` | string or unset | unset | LAN address for the temporary listener; unset auto-detects the primary non-loopback address. |

## Limits and failure behavior

- VCP `0x60` reports the main window only. PIP/PBP can show another source
  while readback still names the main input; do not use shared coordination to
  automate a PIP layout.
- If a third monitor input is selected, both configured machines are non-owners
  until one of their configured inputs returns.
- A DDC read error holds the last ownership verdict. At cold start or after a
  stale observation, ownership stays conservative. An unknown zone acquiring
  ownership does not wake the panel.
- `dormantctl doctor` can report `input_source=skipped` when a controller has no
  usable input-source readback, or `input_source=unreadable` when a read was
  attempted and failed. Fix that before relying on a shared display.
- Discovery is not a heartbeat. Losing an mDNS peer does not change local panel
  ownership, and local presence state is never shared between instances.

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
