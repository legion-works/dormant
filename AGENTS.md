# AGENTS.md — dormant

Guidance for AI coding agents (and humans) working in this repo.

## What this project is

`dormant` — a Rust daemon that blanks OLED screens (PC monitors, TVs) when presence sensors say the room is empty, and wakes them instantly on return. Sensors come in via MQTT, Home Assistant WebSocket, or USB-serial (mmWave radar). Displays are controlled locally (KWin DPMS, DDC/CI) or over the network (Samsung Tizen `KEY_PICTURE_OFF` and IP Control G2 `backlightControl`, HA passthrough, arbitrary commands).

Binaries: **`dormantd`** (daemon) · **`dormantctl`** (CLI + library) · **`dormant-tray`** (KDE tray on Linux, native menu-bar item on macOS).

## Current state

Shipped: **M1** (daemon + sensors + 5 controllers), **M2** (Web UI + config editor), **M3** (KDE tray + render ladder + libmpv screensaver + doctor). The CI matrix is green, the daemon is running on the maintainer's hardware (AOC AGON AG326UZD over DisplayPort, Samsung S90D over the network), and all display controllers have been verified end-to-end. **v0.1.0 is released** — see [ROADMAP.md](./ROADMAP.md) for what's next.

### Branch model

- `dev` is the integration branch. **All PRs target `dev`** (the CI workflow triggers on push to `dev` and on pull requests into `dev`).
- `master` is release-only — a tag push runs the cargo-dist release pipeline. No day-to-day commits land there directly.

### Historical design spec

The original design spec lives at `.opencode/specs/2026-07-04-dormant-design.md`, along with sub-specs for M2/M3 additions. The directory is `.gitignore`d (everything under `.opencode/`) and is **not authoritative** for ship-or-not questions — the code, the CI workflow, and `ROADMAP.md` are. The spec is kept for the design rationale it captures.

## Repo layout (keep this section updated as crates land)

```
crates/dormant-core/       # domain types, traits, config schema/validation, zone+rules engines, state machine, IPC, doctor wire types — pure logic, NO I/O
crates/dormant-sensors/    # one file per sensor: mqtt.rs, ha_ws.rs, usb_ld2410.rs + backoff helper + static registry
crates/dormant-displays/   # one file per controller: command.rs, ddcci.rs, kwin_dpms.rs, samsung_tizen.rs (+ samsung_ip.rs port-1516 transport), ha_passthrough.rs + executor (fallback chain + retry) + registry
crates/dormant-doctor/     # offline + live coalesced hardware/connectivity probes (config, mqtt, ha, usb, ddcci, samsung)
crates/dormant-render/     # Wayland layer-shell render sink: black overlay + libmpv screensaver; Linux-only I/O, non-Linux stub exposes the same surface
crates/dormant-web/        # loopback-only axum HTTP/WS bridge + SPA (crates/dormant-web/webui/) — gated behind the `web-ui` feature of dormantd
crates/dormantd/           # daemon binary: App, event loop, IPC server, single-instance flock, inhibit-activity, reload watcher, optional web UI spawn, logging + systemd/dormant.service
crates/dormantctl/         # CLI binary + library (re-exports the IPC client for dormant-tray and other out-of-process consumers)
crates/dormant-tray/       # Desktop tray applet: KDE StatusNotifierItem (Linux) + native AppKit NSStatusItem (macOS), shared state + IPC loop; systemd and launchd service definitions
```

## Hard conventions (LLM-friendly codebase rules)

These are design commitments, not suggestions — see `ARCHITECTURE.md` for the data-flow map and where-to-find-it guide.

1. **One concept per file, predictable paths.** A sensor lives at `dormant-sensors/src/<name>.rs`, a controller at `dormant-displays/src/<name>.rs`. Soft cap ~300 lines/file.
2. **Grep-stable naming.** Types are `<Name>Source` / `<Name>Controller`. Config `type` strings literally match module names (`type = "usb-ld2410"` ↔ `usb_ld2410.rs`).
3. **Literal string anchors.** Log event names, error codes, config keys are literal strings at the definition site. Never `format!`-construct identifiers; never macro-generate names.
4. **No macro magic.** Sensors/controllers register explicitly in one visible `registry.rs` per crate. No inventory/ctor tricks, minimal proc-macros.
5. **Everything configurable.** No hard-coded timing or policy constants — every knob is a config key with a documented default (`crates/dormant-core/src/config/defaults.rs`).
6. **Fail-safe presence.** Data loss (broker down, USB unplugged, stale sensor) makes a sensor `unavailable`, never `absent`. Default zone policy treats unavailable as present — never blank blind.
7. **Doc comments on public items**; each module opens with a "what lives here" header.
8. **Tests co-located** (`#[cfg(test)]` in-file) + integration tests named by feature (`tests/rule_grace_period.rs`). Property-based tests use `proptest` for inputs with large spaces.

## Build / test / lint

Local gates mirror the CI workflow at `.github/workflows/ci.yml` exactly. Pre-commit hooks mirror the same set.

```bash
# Runtime system deps for build (Debian/Ubuntu). The render features additionally need:
#   libwayland-dev   — Wayland client headers (dormant-render)
#   libmpv-dev pkg-config — libmpv for the screensaver backend
sudo apt install libudev-dev pkg-config

# libudev is sometimes undiscovered by pkg-config depending on layout — set this if
# cargo errors with "udev-sys: failed to run `pkg-config`" during build:
export PKG_CONFIG_PATH=/usr/lib/pkgconfig

# Local gates (run on the current stable toolchain):
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
mdbook build docs     # only needed when docs/src/ changed; CI runs it on every PR

# Less common / CI-only gates (run automatically on PRs):
#   cargo audit         — security advisory DB
#   taplo fmt --check   — TOML formatting (workspace + examples)
#   typos               — spell-check across prose; _typos.toml whitelists library names
```

The full matrix in CI runs fmt, webui (npm lint + build + vitest), clippy, test, render, portability (Windows + macOS `cargo check`), deny, audit, msrv (`cargo check` on Rust 1.88), mqtt-integration (Dockerised mosquitto), docs (rustdoc with `-D warnings`), mdbook, taplo, typos, and a PR-title conventional-commit lint. CI also uploads the built web SPA as an artifact so the Rust crates can `rust-embed` it.

## Commit conventions

Conventional commits (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`, `ci:`) — they drive the generated changelog and the PR-title lint. Scope by crate where useful: `feat(sensors): add LD2412 parser`. PR titles go through the same conventional-commit check in CI.

## Safety rules for agents

- Never weaken the fail-safe presence policy (rule 6) or the wake-retry escalation — a screen that won't wake is the worst failure mode.
- Hardware-dependent behavior (Samsung keys, DDC/CI capabilities, KWin DBus) must be verified via `dormantctl doctor` flows, not assumed — the per-probe capabilities live in `crates/dormant-doctor/src/probes/<target>.rs`, and the SPIKE data for past verification work is in `docs/research/`.
- Config schema changes must stay backward-compatible within a `config_version`; bump the version for breaking changes.
- No telemetry, no phone-home, ever.
