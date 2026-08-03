# Active wear sampling

**What this gives you.** Optional content-weighted OLED wear tracking for one
or more configured displays, using a compositor frame captured from the KDE
Wayland session. Each frame becomes a luma grid for the existing wear ledger;
raw pixels are not stored.

**Scope.** This feature is Linux/KDE Wayland only. It uses the xdg-desktop-
portal ScreenCast flow and samples every display in the per-display
`sampled_displays` list independently; the legacy `sampled_display` singular
form remains accepted for backward compatibility. macOS and Windows remain on
uniform attribution; this feature does not provide capture support for those
platforms, GNOME, X11, or TVs.

## What sampling means

At each wear tick, dormant activates the restored PipeWire stream, captures one
full-resolution compositor frame, reduces it to a 16×9 luma grid, and pauses
the stream again. The grid is resampled to the configured `wear.grid_rows` ×
`wear.grid_cols` ledger grid before attribution. The raw frame is transient;
only the reduced grid remains in memory and contributes to the existing ledger.

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

## Consent and revocation

Enabling grants the daemon's graphical session persistent screen-capture access
through **xdg-desktop-portal ScreenCast** with `persist_mode=2`. Each sampled
display has its own consent record at
`$XDG_STATE_HOME/dormant/screencast-consent-<sanitized-display>.json` (or the
platform state-dir fallback). On the first boot after upgrading from a
singular `sampled_display` config, the legacy un-suffixed
`screencast-consent.json` is copied to the per-display record for that
display; after that one-way copy the per-display file is authoritative and the
legacy file is never read again. The parent directory is mode `0700`; each
consent file is mode `0600`; every record is written with fsync and atomic
rename. The record contains the restore token, the selected display, grant
time, and portal persistent IDs. The token rotates on every reattach. Tokens
and IDs are redacted from logs, status, events, IPC, HTTP, and doctor drafts.

Disable in the configuration to close the session while retaining the record:

```toml
[wear.active_sampling]
enabled = false
```

To cancel a pending flow, close the session, and erase the record, run:

```bash
dormantctl wear disable-sampling --forget
```

Without `--forget`, `disable-sampling` closes the active portal session but
retains its consent record; a later `enable-sampling` reattaches silently
without opening a consent dialog. A pending consent flow remains disabled
after cancellation, so enabling it again requires a saved consent record.

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

## Fallback and diagnostics

Sampling falls back to uniform attribution whenever capture is unavailable,
stale, suspended, denied, timed out, the circuit is open, or reattach
validation fails. The fallback is tagged `uniform`; it does not block blank,
wake, screensaver, reload, or shutdown paths. A changed `sampled_display` also
invalidates the old consent and requires a fresh grant.
When the cooldown retry itself fails, status retains the
`wear_sampling_cooldown` reason while the saved portal session is renegotiated.

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
only attribution for the one selected Linux/KDE Wayland display; all other
displays and non-Linux platforms remain uniform.
