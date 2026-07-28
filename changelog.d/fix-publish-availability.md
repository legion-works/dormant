---
kind: fix
surfaces: []
---
HA entities published by `[publish]` no longer show unavailable: the daemon now publishes a retained `online` on the global availability topic on every connect and re-flushes discovery when Home Assistant announces it is back online on `<discovery_prefix>/status`, so entities the operator deleted (or that HA lost during a restart) re-appear without a daemon restart.
