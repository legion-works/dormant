---
kind: fix
surfaces: [daemon]
issues: []
---
Detail: `daemon.log_level = "debug"` and `"trace"` now scope verbosity to dormant's own crates while keeping third-party logs at an `info` floor; full EnvFilter directives still pass through unchanged.
