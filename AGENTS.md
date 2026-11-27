# AGENTS.md — dormant

Guidance for AI coding agents (and humans) working in this repo.

## What this project is

`dormant` is a Rust daemon that blanks OLED screens when presence sensors report an empty room and wakes them on return. Sensors use MQTT, the Home Assistant WebSocket, or USB serial. Display controllers include DDC/CI, KWin DPMS, macOS gamma and sleep, Samsung Tizen, Home Assistant service calls, and shell commands.

Binaries: **`dormantd`** (daemon) · **`dormantctl`** (CLI + library) · **`dormant-tray`** (KDE tray on Linux, native menu-bar item on macOS).

## Current state

The daemon and CLI support Linux, macOS, and source builds on Windows. The tray supports Linux and macOS. The Wayland render backend is Linux-only. See [CHANGELOG.md](./CHANGELOG.md) for releases, [Cargo.toml](./Cargo.toml) for the workspace version, and [ROADMAP.md](./ROADMAP.md) for planned work.

### Branch model

- `dev` is the integration branch. **All PRs target `dev`** (the CI workflow triggers on push to `dev` and on pull requests into `dev`).
- `master` is release-only — a tag push runs the cargo-dist release pipeline. No day-to-day commits land there directly.

### Historical design spec

The local design specs under `.opencode/specs/` are not in Git. Use the source, CI workflow, and `ROADMAP.md` for current behavior and plans.

## Repo layout (keep this section updated as crates land)

```
crates/dormant-core/       # domain types, traits, config schema/validation, zone+rules engines, state machine, IPC, doctor wire types — pure logic, NO I/O
crates/dormant-sensors/    # one file per sensor: mqtt.rs, ha_ws.rs, usb_ld2410.rs + backoff helper + static registry
crates/dormant-displays/   # controllers, fallback executor, registry, DDC bus locks, Linux DRM connector identity
crates/dormant-doctor/     # offline and live probes, including macOS and Windows idle checks
crates/dormant-render/     # Wayland layer-shell render sink: black overlay + libmpv screensaver; Linux-only I/O, non-Linux stub exposes the same surface
crates/dormant-web/        # axum HTTP/WS bridge + SPA (crates/dormant-web/webui/); loopback by default; dormantd feature: web-ui
crates/dormantd/           # daemon: App, event loop, Unix socket or Windows named pipe, single-instance lock, activity, hooks, reload, wear, optional web UI
crates/dormantctl/         # CLI binary + library (re-exports the IPC client for dormant-tray and other out-of-process consumers)
crates/dormant-tray/       # Desktop tray applet: KDE StatusNotifierItem (Linux) + native AppKit NSStatusItem (macOS), shared state + IPC loop; systemd and launchd service definitions
```

## Hard conventions (LLM-friendly codebase rules)

These are design commitments, not suggestions — see `ARCHITECTURE.md` for the data-flow map and where-to-find-it guide.

1. **One concept per file, predictable paths.** A sensor lives at `dormant-sensors/src/<name>.rs`, a controller at `dormant-displays/src/<name>.rs`. Soft cap ~300 lines/file.
2. **Grep-stable naming.** Name types `<Name>Source` or `<Name>Controller`. Match registry keys to module names, with underscores for hyphens. The `ha` sensor uses `ha_ws.rs`.
3. **Literal string anchors.** Log event names, error codes, config keys are literal strings at the definition site. Never `format!`-construct identifiers; never macro-generate names.
4. **No macro magic.** Sensors/controllers register explicitly in one visible `registry.rs` per crate. No inventory/ctor tricks, minimal proc-macros.
5. **Configurable timing and policy.** Use config keys for user-adjustable timing and policy. Document their defaults in `crates/dormant-core/src/config/defaults.rs`.
6. **Fail-safe presence.** Data loss (broker down, USB unplugged, stale sensor) makes a sensor `unavailable`, never `absent`. Default zone policy treats unavailable as present — never blank blind.
7. **Doc comments on public items**; each module opens with a "what lives here" header.
8. **Tests co-located** (`#[cfg(test)]` in-file) + integration tests named by feature (`crates/dormantd/tests/boot_rollback.rs`). Property-based tests use `proptest` for inputs with large spaces.

## Build / test / lint

Use `.github/workflows/ci.yml` for the complete CI matrix. Platform jobs select different feature sets.

```bash
# Runtime system deps for build (Debian/Ubuntu). The render features additionally need:
#   libwayland-dev   — Wayland client headers (dormant-render)
#   libmpv-dev pkg-config — libmpv for the screensaver backend
#   libpipewire-0.3-dev — PipeWire client headers (dormantd active sampling)
sudo apt install libudev-dev libpipewire-0.3-dev pkg-config

# libudev is sometimes undiscovered by pkg-config depending on layout — set this if
# cargo errors with "udev-sys: failed to run `pkg-config`" during build:
export PKG_CONFIG_PATH=/usr/lib/pkgconfig

# Local gates (run on the current stable toolchain):
cargo fmt --all -- --check
bash scripts/gates/clippy.sh
bash scripts/gates/rust-tests.sh
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
mdbook build docs     # only needed when docs/src/ changed; CI runs it on every PR

# Less common / CI-only gates (run automatically on PRs):
#   taplo fmt --check   — TOML formatting (workspace + examples)
#   typos               — spell-check across prose; _typos.toml whitelists library names
```

CI runs workspace tests on Linux, macOS, and Windows. Windows builds, tests, and runs Clippy with `web-ui`. The Linux and macOS MSRV jobs use Rust 1.88. CI also checks the web UI, render features, MQTT integration, documentation, dependency policy, secrets, spelling, and PR titles.

## Commit conventions

Conventional commits (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`, `ci:`) — they drive the generated changelog and the PR-title lint. Scope by crate where useful: `feat(sensors): add LD2412 parser`. PR titles go through the same conventional-commit check in CI.

## Safety rules for agents

- Never weaken the fail-safe presence policy (rule 6) or the wake-retry escalation — a screen that won't wake is the worst failure mode.
- Hardware-dependent behavior (Samsung keys, DDC/CI capabilities, KWin DBus) must be verified via `dormantctl doctor` flows, not assumed — the per-probe capabilities live in `crates/dormant-doctor/src/probes/<target>.rs`, and the SPIKE data for past verification work is in `docs/research/`.
- Config schema changes must stay backward-compatible within a `config_version`; bump the version for breaking changes.
- No telemetry, no phone-home, ever.
