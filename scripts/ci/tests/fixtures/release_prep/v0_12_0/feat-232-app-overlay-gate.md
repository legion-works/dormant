---
kind: improvement
surfaces: []
---

The source gate for active wear sampling on remote-controlled TVs now also
checks for installed Tizen apps that own the panel without changing the
reported input source (issue #232 — Netflix fullscreen kept the S90D's
`inputSourceControl` at `HDMI4`, silently misattributing the local
compositor's frames to on-Netflix panel time). A new
`[displays.<id>.sampling] watched_apps = ["<app-id>", ...]` config key
probes `GET http://<host>:8001/api/v2/applications/<id>` for each configured
id on the existing 15-second source-poll cadence; a `visible: true`
response forces the gate to `mismatched` even when `expected_source`
matches. Current Tizen firmware has no reliable enumeration endpoint, so
the catalog is user-extensible rather than hardcoded.

Detail: the probe is fail-safe toward attribution — an unreachable 8001
endpoint never flips the gate on its own (it degrades to the input-only
verdict, mirroring the existing source-gate fail-safe). The two probes
ride a single `source_poll_interval` timer; a confirmed `Visible` app
short-circuits the cycle so the input read is skipped that tick.
