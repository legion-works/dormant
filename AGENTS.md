# AGENTS.md — dormant

Guidance for AI coding agents (and humans) working in this repo.

## What this project is

`dormant` — a Rust daemon that blanks OLED screens (PC monitors, TVs) when presence sensors say the room is empty, and wakes them instantly on return. Sensors come in via MQTT, Home Assistant WebSocket, or USB-serial (mmWave radar). Displays are controlled locally (KWin DPMS, DDC/CI) or over the network (Samsung Tizen `KEY_PICTURE_OFF`, LG webOS, HA passthrough, arbitrary commands).

Binaries: `dormantd` (daemon) · `dormantctl` (CLI) · `dormant-tray` (tray app, M3).

## Current state

**Design phase.** No code yet. The authoritative design spec is
`.opencode/specs/2026-07-04-dormant-design.md` — read it before proposing or writing anything.
Milestones M1/M2/M3 and out-of-scope items are defined there; do not build ahead of the current milestone.

## Repo layout (planned — keep this section updated as crates land)

```
crates/dormant-core/       # domain types, traits, zone+rules engines, state machine — pure logic, NO I/O
crates/dormant-sensors/    # one file per sensor: mqtt.rs, ha_ws.rs, usb_ld2410.rs
crates/dormant-displays/   # one file per controller: kwin_dpms.rs, ddcci.rs, samsung_tizen.rs, ...
crates/dormantd/           # daemon binary: config, event loop, control surfaces
crates/dormantctl/         # CLI binary + library (re-exports the IPC client for dormant-tray)
crates/dormant-tray/       # KDE StatusNotifierItem applet (M3, Linux only)
webui/                     # SPA (M2), embedded into dormantd
docs/                      # mdBook + docs/adr/ decision records
```

## Hard conventions (LLM-friendly codebase rules)

These are design commitments, not suggestions — see spec §9:

1. **One concept per file, predictable paths.** A sensor lives at `dormant-sensors/src/<name>.rs`, a controller at `dormant-displays/src/<name>.rs`. Soft cap ~300 lines/file.
2. **Grep-stable naming.** Types are `<Name>Source` / `<Name>Controller`. Config `type` strings literally match module names (`type = "usb-ld2410"` ↔ `usb_ld2410.rs`).
3. **Literal string anchors.** Log event names, error codes, config keys are literal strings at the definition site. Never `format!`-construct identifiers; never macro-generate names.
4. **No macro magic.** Sensors/controllers register explicitly in one visible `registry.rs` per crate. No inventory/ctor tricks, minimal proc-macros.
5. **Everything configurable.** No hard-coded timing or policy constants — every knob is a config key with a documented default (spec §6).
6. **Fail-safe presence.** Data loss (broker down, USB unplugged, stale sensor) makes a sensor `unavailable`, never `absent`. Default zone policy treats unavailable as present — never blank blind.
7. **Doc comments on public items**; each module opens with a "what lives here" header.
8. **Tests co-located** (`#[cfg(test)]` in-file) + integration tests named by feature (`tests/rule_grace_period.rs`).

## Build / test / lint (once scaffolded)

```
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

CI enforces all of the above plus cargo-audit, taplo, typos, MSRV, and conventional-commit PR titles. Run the full set locally before committing; pre-commit hooks mirror CI.

## Commit conventions

Conventional commits (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`, `ci:`) — they drive the generated changelog. Scope by crate where useful: `feat(sensors): add LD2412 parser`.

## Safety rules for agents

- Never weaken the fail-safe presence policy (rule 6) or the wake-retry escalation — a screen that won't wake is the worst failure mode.
- Hardware-dependent behavior (Samsung keys, DDC/CI capabilities, KWin DBus) must be verified via `dormantctl doctor` flows, not assumed — see spec §11 for the open verification items.
- Config schema changes must stay backward-compatible within a `config_version`; bump the version for breaking changes.
- No telemetry, no phone-home, ever (spec §10).
