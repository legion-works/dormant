# macOS display-selector contract — decision record (Task 4)

Decision: output = "cg:<lowercase-CFUUID>" — ratified 2026-07-16.

Canonical config form for per-display macOS targeting (`macos-gamma-black`,
M2 `RenderBlack`): `output = "cg:<lowercase-cfuuid>"`, where the UUID comes
from `CGDisplayCreateUUIDFromDisplayID`. A `MacosDisplayCatalog` resolves
UUID → current `CGDirectDisplayID` at every probe/build, so numeric-ID churn
across reboots/reconnects is harmless. `macos-gamma-black` requires a specific
`cg:` output (missing output = hard config error, never a main-display
fallback). `macos-display-sleep` is the explicit exception (absent or
`"all"`). `ddcci` keeps `ddc_display`; the DDC identifier and the Quartz
output identifier are never conflated.

## Evidence

Probe: `CGGetOnlineDisplayList` + `CGDisplayCreateUUIDFromDisplayID` +
`CFUUIDCreateString`, run over `ssh mac` (macOS 26.5, MacBook Pro M3 Pro,
AOC AG326UZD external over USB-C).

Script correction vs the drafted probe (found by running it on target):
`CGDisplayCreateUUIDFromDisplayID` is not in scope without `import ColorSync`,
and on current SDKs it returns `Unmanaged<CFUUID>?` (optional — same for the
`CFUUIDCreateString` bridging). The working probe guards the optional and
prints `uuid=NONE` for a display with no UUID rather than crashing.

### Observation 1 — 2026-07-16, steady state

```
id=2 uuid=aabd49bb-d732-41be-adaf-d30d188a40c9 builtin=0 main=1   (AOC AG326UZD, external, USB-C)
id=1 uuid=37d8832a-2d66-02ca-b9f7-8f30a301b230 builtin=1 main=0   (built-in)
```

Corroboration: `m1ddc display list` independently reports the same AOC UUID
(`AABD49BB-D732-41BE-ADAF-D30D188A40C9`) via the IOAVService surface — two
unrelated APIs agree on the identity.

Main-display instability, observed same-day: at the morning M0 spike the
BUILT-IN display was `main`; by this probe the EXTERNAL was `main=1`. Any
selector keyed on the main display would have broken twice in one day —
hard evidence for the plan's "never key on main" trap.

### Observation 2 — stability across reconnect/reboot

OPERATOR-GATED, still open: re-run the probe after a USB-C replug or reboot
and confirm `aabd49bb-…` is unchanged for the AOC. If the UUID proves
unstable, the decision reverts to unresolved per the plan's stop rule (no
name-based fallback may be invented).

## Validation semantics (pinned)

- `cg:` selectors are validated for shape (lowercase UUID) at config load.
- Unknown/offline UUID at build time: controller probe fails fail-safe
  (display treated as not-mine; never blank blind) — resolution retries on
  the next catalog refresh.
- No main-display fallback exists, deliberately.
