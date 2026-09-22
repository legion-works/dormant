---
kind: capability
surfaces: [daemon, displays]
issues: [265]
---
User can now: configure the `ddcci` display controller on Windows, which is the platform's only blanking path and is audio-safe by mechanism.

Detail: `ddc-hi` already carried a Windows backend (`ddc-winapi`); the dependency and the `RealVcp` gates were pinned to Linux and macOS. Widening them enables DDC/CI on Windows, and `dormantctl doctor ddcci` is available there to verify the hardware. Behaviour on real Windows hardware is unverified.
