---
kind: capability
surfaces: [daemon]
issues: [265]
---
User can now: run `dormantd` on Windows without a second instance starting and fighting the same displays' DDC bus.

Detail: The Windows single-instance guard now takes a real exclusive `LockFileEx` byte-range lock on the per-user-session lock file instead of returning success unconditionally. The handle closes on process exit, so the lock is crash-safe exactly like the unix `flock` arm.