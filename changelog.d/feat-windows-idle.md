---
kind: capability
surfaces: [daemon]
issues: [265]
---
User can now: keep a Windows machine's displays awake while they are actively using it, via a `GetLastInputInfo` user-activity idle source.

Detail: `idle_source = "auto"` (and `"windows"`) now selects a real Windows idle source instead of the inert non-Linux DBus stub, so a rule declaring `inhibitors = ["user-activity"]` actually inhibits. The 32-bit `GetTickCount` wrap is handled with `wrapping_sub`, and the source reuses the macOS frozen/sanity-cap/startup-grace guard. `GetLastInputInfo` sees only the calling session and not input into elevated windows from a non-elevated process. Behaviour on real Windows hardware is unverified until CI runs.