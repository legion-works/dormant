# Active wear sampling

**What this gives you.** Optional content-weighted OLED wear tracking for one
or more configured displays. A compositor frame is captured from the KDE
Wayland session (a local monitor) or from a declared compositor output behind
an HDMI-connected Samsung TV, reduced to a luma grid for the existing wear
ledger, and attributed only while the intended source is actually visible.
Raw pixels are never stored.

**When to use it.** When uniform brightness-weighted on-hours is too coarse
for a panel you care about — a static UI on one half of the screen ages
differently from a full-screen movie, and a TV showing Netflix instead of your
HDMI input should not count as local-frame wear at all. Not for you if you have
no KDE Wayland session (the capture path is Linux/KDE Wayland only), or if you
only want the existing uniform ledger — active sampling is opt-in and changes
attribution, not blank/wake timing. GNOME, X11, macOS, and Windows stay on
uniform attribution; this feature does not provide capture for those
platforms.

**Quick setup.** Declare a compositor output and pin the source the sampler
should expect, then opt the display into the wear section's sampled list:

```toml
[displays.tv]
controllers = ["samsung-tizen"]
host = "10.1.1.7"
blank_mode = "screen_off_audio_on"
compositor_output = "HDMI-A-1"

[displays.tv.sampling]
expected_source = "HDMI4"
source_poll_interval = "15s"

[wear.active_sampling]
enabled = true
sampled_displays = ["monitor", "tv"]
```

Verify the sampler can see the TV's input:

```bash
dormantctl status          # look for "(source: matched)" on the TV row
dormantctl wear enable-sampling --display tv
```

---

Full config reference · behaviour details · failure modes and troubleshooting

## Scope and platforms

Active sampling captures compositor frames on Linux/KDE Wayland through the
xdg-desktop-portal ScreenCast flow. It samples every display in the
per-display `sampled_displays` list independently; the legacy
`sampled_display` singular form remains accepted for backward compatibility.

Two display classes are sampling-eligible:

- A **local render-eligible** display — one with a local controller
  (`kwin-dpms`, `ddcci`, or `command`) in its `controllers` list and an
  `output` set. The sampler captures the compositor frame driving that panel
  directly.
- A **remote-only TV** that declares `compositor_output` and a
  `[displays.<id>.sampling]` table. Today the only shipping source reader is
  `samsung-tizen` over Samsung IP Control (port 1516); no other TV vendor or
  transport is supported. The sampler captures the local compositor output
  that feeds the TV's HDMI input, not the TV's own framebuffer.

`compositor_output` opts a remote display into the sampling path. It is
separate from the `kwin-dpms` `output` key (which names the local render
target) and does not enable any render-ladder stage — a remote-only TV
still cannot run `render_black` or `render_screensaver` stages.

## What sampling means

At each wear tick, dormant activates the restored PipeWire stream, captures
one full-resolution compositor frame, reduces it to a 16×9 luma grid, and
pauses the stream again. The grid is resampled to the configured
`wear.grid_rows` × `wear.grid_cols` ledger grid before attribution. The raw
frame is transient; only the reduced grid remains in memory and contributes to
the existing ledger.

The frame is sampled once per `wear.sample_interval` (default `60s`). This is
a single-frame-per-tick approximation, not integration over the whole
interval. It stands in for the content shown during that window. A future
version may average multiple frames; v1 does not.

Content luma is measured before the panel LUT/night-color transform. It is a
host-side estimate, not calibrated panel luminance or panel telemetry. Each
sampled display's `total_on_hours` becomes luma-weighted, so it will generally
be smaller and is not comparable 1:1 with pre-M2 or unsampled ledgers.

Attribution is tagged `sampled` or `uniform` for status and wear events. The
ledger schema does not change. The existing phase rules still win: a frame
never overrides a blank, wake, grace, or render-stage decision.

## Defaults and configuration

Active sampling is opt-in and disabled by default. The complete configuration
is under `[wear.active_sampling]`. `config_version = 1` accepts the legacy
singular `sampled_display` OR the canonical plural `sampled_displays`; supply
both and the daemon rejects the config at parse time:

```toml
[wear]
sample_interval = "60s" # also drives active-frame cadence

[wear.active_sampling]
enabled = false
sampled_displays = ["monitor", "sidecar"]  # canonical plural form
stream_mode = "warm"
capture_timeout = "2s"
failure_threshold = 5
circuit_reset_after = "5m"
```

| Key | Default | Meaning |
|---|---|---|
| `wear.active_sampling.enabled` | `false` | Opt in to compositor sampling |
| `wear.active_sampling.sampled_display` | unset | Legacy singular form: one configured, wear-tracked display; required when enabled and the plural list is absent |
| `wear.active_sampling.sampled_displays` | unset | Canonical plural form: one or more configured, wear-tracked displays; required when enabled and the singular form is absent |
| `wear.active_sampling.stream_mode` | `"warm"` | Keep the paused stream attached, or use `"per-tick"` |
| `wear.active_sampling.capture_timeout` | `"2s"` | Maximum time for one capture; valid range is `1s`–`30s` |
| `wear.active_sampling.failure_threshold` | `5` | Consecutive failures before the circuit opens |
| `wear.active_sampling.circuit_reset_after` | `"5m"` | Delay before retrying an open circuit |

Each id in `sampled_displays` drives an independent sampler with its own
consent record, PipeWire stream, lifecycle status, and cancellation token, so
one display's consent failure or open circuit does not affect the others.
Consent records are per-display files (see [Consent and
revocation](#consent-and-revocation)).

The active capture timeout must be no more than half of `wear.sample_interval`.
When active sampling is enabled, `wear.sample_interval` must therefore be at
least `2s`; there is no separate active-sampling cadence knob. Existing wear
defaults remain `wear.grid_rows = 9`, `wear.grid_cols = 16`, and
`wear.sample_interval = "60s"`.

`stream_mode = "warm"` is the shipped default. The warm-paused stream measured
zero marginal idle cost in the daemon-identity gate of the M2 capture spike
(`docs/research/2026-07-31-m2-capture-spike.md`: `kwin_wayland` −0.089 and
PipeWire −0.008 percentage points over 30-minute windows), with pause→resume
p95 of 14.4 ms. `"per-tick"` tears down the stream after each
capture and recreates it on the next tick, without requiring consent again.

### Per-display sampling table

A remote-only TV declares its source gate under
`[displays.<id>.sampling]` (see [Configuration](./configuration.md)):

| Key | Default | Meaning |
|---|---|---|
| `expected_source` | unset | Input source label the TV must report for a capture to count. Matched exactly and case-sensitively; required when `compositor_output` is set |
| `source_poll_interval` | `"15s"` | Cadence for the Samsung IP Control source read; valid range `5s`–`5m` (inclusive) |
| `stream_mode` | unset | Per-display override of `[wear.active_sampling] stream_mode`: `"warm"` or `"per-tick"`. Unset inherits the wear section's mode |
| `watched_apps` | seeded catalog | Tizen app ids the source gate probes for screen ownership via the unauthenticated `GET http://<host>:8001/api/v2/applications/<id>` endpoint. A `visible: true` response forces the gate to `mismatched` even when `expected_source` matches — apps own the panel without flipping `inputSourceControl`. **Absent** key inherits the daemon-shipped seed (`defaults::WEAR_SAMPLING_DEFAULT_WATCHED_APPS` — common streamers like `Netflix`, `YouTube`, `Prime Video`, `Disney+`, etc.) so a stock TV config suspends spatial attribution under installed apps out of the box. **Empty array** `watched_apps = []` is the explicit opt-out — pure input-only gate, no app-visibility probe at all. Operators can set an explicit list to override the seed; current Tizen firmware has no reliable enumeration endpoint, so the operator's catalog is the long-term source of truth. |

`expected_source` is required once `compositor_output` is set — validation
rejects a remote-only display that opts into sampling without pinning the
source, because the source gate would have nothing to compare against. The
`expected_source` string is free-form but must match the label Samsung IP
Control returns verbatim (e.g. `"HDMI4"`, not `"hdmi 4"`); the match is exact
and case-sensitive.

`stream_mode = "per-tick"` is the two-stream fallback. Sampling two displays
at once spawns one PipeWire stream per display; a TV that overrides to
`per-tick` tears its stream down after each capture instead of holding a
second warm stream alongside the monitor's, so the second concurrent stream
exists only for the capture window. The monitor inherits the global `warm`
mode unchanged. The two-stream resource cost is measured in
[Two-stream active sampling](../research/active-sampling-two-streams.md);
until those measurements land, multi-display sampling stays opt-in.

## The source gate

For a TV carrying `expected_source`, the daemon polls Samsung IP Control
(`inputSourceControl`) and, when `watched_apps` is non-empty, the
Tizen REST endpoint on port 8001 for each configured app id; both probes
ride the same `source_poll_interval` cycle (one timer, no second poll).

| State | Tag | When | Capture | Attribution |
|---|---|---|---|---|
| Matched | `matched` | The TV reports `expected_source` AND no watched app is currently visible | Runs | `sampled` (spatial, luma-weighted) |
| Mismatched | `mismatched` | The TV reports a different source OR a watched app is currently visible (NetFlix, YouTube, etc.) | Skipped | `uniform` tagged `source_mismatch` |
| Unknown | `unknown` | Both the input probe AND the app probes could not establish a verdict (network/parse/timeout) | Skipped | `uniform` tagged `source_unknown` |

The gate is fail-safe toward attribution, never toward a zero span: while the
TV is on another source the panel is still ON and still aging, so the tick
degrades to uniform attribution tagged `source_mismatch` — spatial
attribution is suppressed, but the on-hours still accrue. The same applies to
`source_unknown`. A matched gate runs the capture and attributes spatially;
a display with no `[displays.<id>.sampling]` table (a local monitor) has no
gate and is treated as permanently matched.

### Default seeded catalog (fail-safe opt-out via `[]`)

The field defaults to the daemon-shipped seed
(`defaults::WEAR_SAMPLING_DEFAULT_WATCHED_APPS` — common streamers like
`Netflix`, `YouTube`, `Prime Video`, `Disney+`, etc.) when the
`[displays.<id>.sampling]` table is present but `watched_apps` is omitted.
The seed is the fail-safe direction: a stock TV config that declares
`expected_source` gets app detection out of the box, so launching Netflix
on the operator's S90D suspends spatial attribution immediately without
requiring the operator to enumerate their installed app set first (current
Tizen firmware has no reliable enumeration endpoint — see issue #232).

The opt-out is **explicit** `watched_apps = []`: serde's per-field default
function only fires for absent keys, so an empty array deserializes as an
empty `Vec` and the gate runs in pure input-only mode (the pre-#232
behavior). Operators who want to extend the catalog beyond the seed set
the key to an explicit list — serde honors the operator's value over the
default. To disable the probe entirely, write `watched_apps = []`.

### Why a second probe (issue #232)

The input-source check on port 1516 (`inputSourceControl`) is necessary but
not sufficient. Tizen apps (Netflix, YouTube, Prime Video, etc.) own the
panel without flipping the reported `inputSource` value — the operator's
S90D kept reporting `HDMI4` while Netflix was fullscreen, which silently
misattributed the local compositor's frames to panel-on-Netflix time. The
8001 probe (`/api/v2/applications/{id}`) returns `visible: true` for any
app currently owning the screen; the gate treats a positive result as
"the panel is not showing our HDMI source" regardless of what
`inputSourceControl` says.

The app-visibility probe rides the existing 15-second source-poll cadence
with a tight per-app timeout (2s) so a configured catalog of 5–8 apps fits
inside one cycle; a confirmed `Visible` app short-circuits the cycle so the
input read is skipped that tick — the gate is going to flip on this poll and
one fewer port-1516 round-trip is one less load on the TV.

### Fail-safe direction under app-visibility uncertainty

The probe has three possible outcomes (`Visible`, `NotVisible`, `Unknown`).

- **`Visible` forces `Mismatched`** — even when the input matches. The
  wear-ledger integrity is the protected resource here: a false match
  with a visible app silently corrupts the spatial attribution, while a
  false mismatch only costs one cycle of uniform (still-on) attribution.
  Spec invariant from `#232`: wear-ledger integrity beats sampling uptime.
- **`NotVisible` degrades to the input-only verdict** — the steady state on
  HDMI4.
- **`Unknown` (network unreachable, parse failure, timeout) does NOT flip the
  gate on its own.** When the input probe also failed the cycle is fully
  degraded (`Unknown { reason: "poll_failed" }`). When the input probe
  matched, an `Unknown` app degrades silently to the input-only `Matched`
  verdict — preserves uniform attribution, never fabricates mismatch.

This mirrors the source-gate's own fail-safe analysis: a screen that is
likely-but-not-certainly not showing our content still ages uniformly; a
screen that might be showing something we don't own cannot be recorded as
ours.

The poller runs only while the sampler is `Streaming` and bound to the
matching expectation. Adding, removing, or changing `expected_source` tears
down and re-spawns the poller without touching the consent record or closing
the portal session — a gated capture is not a gated consent. No capture
ticks fire while the gate is `mismatched` or `unknown`: the latest sampled
grid is cleared on every transition away from matched so a stale grid is never
promoted to a fresh attribution.

The `WearSamplingSourceGate` daemon event fires only on a full gate change
(`matched` → `mismatched` → `unknown` → …), not on every steady-state poll, so
a long run of mismatched polls emits one event, not one per tick.

## Consent and revocation

Enabling grants the daemon's graphical session persistent screen-capture
access through **xdg-desktop-portal ScreenCast** with `persist_mode=2`. Each
sampled display has its own consent record at
`$XDG_STATE_HOME/dormant/screencast-consent-<sanitized-display>.json` (or the
platform state-dir fallback). On the first boot after upgrading from a
singular `sampled_display` config, the legacy un-suffixed
`screencast-consent.json` is copied to the per-display record for that
display; after that one-way copy the per-display file is authoritative and the
legacy file is never read again. The parent directory is mode `0700`; each
consent file is mode `0600`; every record is written with fsync and atomic
rename. Tokens and portal IDs are redacted from logs, status, events, IPC,
HTTP, and doctor drafts.

The record stores, in order: the restore `token`, the `sampled_display` id,
the `granted_at` timestamp, the `portal_persistent_ids`, the granted
`granted_width` and `granted_height`, the logical `stream_position` `(x, y)`
the compositor reported at grant time, and the `compositor_output` name the
operator bound the grant to. The last two bind the grant to the intended
panel:

- **`compositor_output` drift invalidates consent.** A reconfigure that
  moves the sampler to a different compositor output (e.g. `HDMI-A-1` →
  `HDMI-A-2`) no longer matches the stored record; the daemon treats this as
  `wear_sampling_display_changed`, falls back to uniform attribution, and
  requires a fresh grant. The same happens when `compositor_output` is added
  to a display whose old record predates the field.
- **`stream_position` is the second binding signal.** Two same-resolution 4K
  monitors share a persistent id and dimensions; the compositor-reported
  position is the only signal left to keep them apart. When both the recorded
  and observed positions are present and differ, reattach fails with
  `wear_sampling_wrong_monitor`. **Position omission fallback:** when either
  side is absent — older records, or older compositors that omit the `position`
  field — the position check is skipped and the dimension check alone remains
  binding. Older on-disk records deserialize with both new fields as `None`, so
   no migration is required.

### One dialog at a time

The daemon serializes portal consent across all displays and entry points
(CLI, web UI, and tray). While one dialog is open, an enable for another
display fails with `wear_sampling_consent_busy: <holder-display>`; finish or
dismiss the named dialog, then retry.

The portal owns the dialog and cannot identify its target, so two simultaneous
dialogs have the same title and tile list. Serializing them and naming the
target makes a grant unambiguous. Before the dialog opens, `dormantctl wear
enable-sampling` prints `waiting for consent dialog — pick the tile for display
'tv' (HDMI-A-1) — up to 5 minutes`; the parenthesized name is the display's
`compositor_output`, and is omitted when that key is unset. The
`wear_sampling_consent_target` log event carries the same `display` and
`compositor_output` for a non-interactive record.

`wear_sampling_wrong_monitor` still rejects a crossed grant after it happens;
the serialized flow makes that crossed grant less likely rather than replacing
the guard.

Disable in the configuration to close the session while retaining the record:

```toml
[wear.active_sampling]
enabled = false
```

To cancel a pending flow, close the session, and erase the record, run:

```bash
dormantctl wear disable-sampling --display <id> --forget
```

`--display` is required whenever more than one display has sampling
configured; with exactly one it may be omitted. The bare form on a
multi-display config fails with `multiple displays configured — pass
--display to pick one` rather than acting on all of them, so a single
command never closes a session you did not name.

`--forget` is the recovery path for a drifted or stale consent record: it
deletes the on-disk record so the next `enable-sampling` opens a fresh portal
dialog instead of failing reattach against a record bound to a different
output or position. Without `--forget`, `disable-sampling` closes the active
portal session but retains its consent record; a later `enable-sampling`
reattaches silently without opening a consent dialog. A pending consent flow
remains disabled after cancellation, so enabling it again requires a saved
consent record.

The compositor grant can also be revoked outside dormant at **KDE System
Settings → Applications → Screen Sharing permissions**. Revoking there makes
the next attach fall back to uniform attribution and request consent only via
the explicit enable flow; boot and reload never pop a dialog.

## Enable flow

The web WearCard offers **Enable sampling** only when the status is
`NeedsConsent`. It starts the same daemon-owned flow as the CLI and polls until
the status is terminal. The operator chooses the display in the portal dialog;
the grant is checked against that display's persistent ID and dimensions before
anything is stored.

The CLI command is:

```bash
dormantctl wear enable-sampling
```

It waits up to five minutes and prints a hint line if the portal dialog is not
visible. A missing graphical session, disabled config, an in-flight flow, a
denial, timeout, or monitor mismatch fails explicitly. On later daemon starts,
a valid record is reattached silently.

## Status and logs

The per-display sampler status surfaces the redacted source gate alongside the
lifecycle state:

- **`dormantctl status`** appends `(source: <gate>)` to the sampling line —
  e.g. `sampling: streaming (age: 1m 35s) (source: mismatched)`. The gate is
  one of `matched`, `mismatched`, or `unknown`, and is omitted when the display
  carries no gate configuration (a local monitor).
- **`dormantctl wear enable-sampling` / `disable-sampling`** print a
  `source: <gate>` hint line for the selected display before sending the
  request, so the operator can see why a reattach is about to fail.
- **`GET /api/wear`** adds `source_gate` and `uniform_reason` fields per
  display to the wear summary. `source_gate` is the stable tag (`matched`,
  `mismatched`, `unknown`); `uniform_reason` is the reason the current
  interval is uniform while sampling is degraded (e.g. `source_mismatch`,
  `source_unknown`). Both are absent when `None` so older UIs keep parsing.
- **WearCard** renders a dedicated warning line for a gated TV:
  `not sampling — TV is on another source` for `mismatched`, and
  `not sampling — TV source unavailable` for `unknown`. A matched gate and a
  no-gate monitor fall through to the normal sampling label.
- **`wear_sampling_source_gate`** daemon event (web event log, `dormantctl
  watch`) fires on every full gate change with the display, the new state,
  and the observed source label for a mismatched poll.

The daemon log emits four transition events on a gate change, plus a
steady-state debug poll and a per-capture timing trace:

| Event | Level | Meaning |
|---|---|---|
| `wear_sampling_source_matched` | info | The gate returned to `matched` |
| `wear_sampling_source_mismatch` | warn | The TV reported a source other than `expected_source` (logs `observed` and `expected`) |
| `wear_sampling_source_poll_failed` | warn | The Samsung IP Control read failed (logs `reason`) |
| `wear_sampling_source_unknown` | warn | The source could not be established safely, e.g. an empty response (logs `reason`) |
| `wear_sampling_source_poll` | debug | Every steady-state poll observation (logs `expected`, `state`, `observed`) |
| `wear_sampling_capture_timing` | debug | Per-capture monotonic sequence with `stage = requested` / `frame_ready` / `reduction_complete` and `elapsed_ms` on the latter two; gated ticks emit none |

## Fallback and diagnostics

Sampling falls back to uniform attribution whenever capture is unavailable,
stale, suspended, denied, timed out, the circuit is open, or reattach
validation fails. The fallback is tagged `uniform`; it does not block blank,
wake, screensaver, reload, or shutdown paths. A changed `sampled_display` also
invalidates the old consent and requires a fresh grant.

Source-gate fallback is additive: a `mismatched` or `unknown` gate degrades
the tick to uniform attribution tagged `source_mismatch` or `source_unknown`
respectively, and the gate reason outranks `suspended` — a gated-but-suspended
status still tags the gate, because the panel keeps aging either way. No
capture tick fires while gated; the on-hours still accrue as uniform for the
full sample interval. When the cooldown retry itself fails, status retains the
`wear_sampling_cooldown` reason while the saved portal session is
renegotiated.

The `wear-sampling` doctor probe is **live-only**. It runs from the web Doctor
view or through the daemon's IPC service and checks the running sampler; there
is deliberately no `dormantctl doctor wear-sampling` offline arm because an
offline process cannot inspect daemon-owned consent, PipeWire state, or the
live portal session without creating a second capture lifecycle.

## Privacy and non-goals

No image-like data is persisted, logged, or exposed over IPC/HTTP. The
consent record is the sensitive artifact and uses owner-only permissions and
redaction. Active sampling does not perform RGB/channel attribution, panel-
type weighting, compensation actions, or automated cadence tuning. It changes
only attribution for the sampled displays — local Linux/KDE Wayland monitors
and source-gated Samsung TVs — while all other displays and non-Linux
platforms remain uniform.
