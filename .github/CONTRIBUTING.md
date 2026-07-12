# Contributing

## Branch model

- **All pull requests target `dev`.** The CI workflow at `.github/workflows/ci.yml` is configured to run on push to `dev` and on pull requests *into* `dev`. PRs against `master` will not trigger CI and will not be merged.
- `master` is release-only — it advances via a tag push that triggers the cargo-dist release pipeline. Do not commit day-to-day work there directly.

## Development setup

1. Install Rust via [rustup](https://rustup.rs) — the MSRV is 1.88.
2. Install system dependencies:

   ```bash
   # Debian/Ubuntu
   sudo apt install libudev-dev pkg-config
   ```

   The `PKG_CONFIG_PATH` may need to be set explicitly if `pkg-config` cannot find `libudev`:
   ```bash
   export PKG_CONFIG_PATH=/usr/lib/pkgconfig
   ```

3. For the MQTT integration test, install Docker (the CI test uses `eclipse-mosquitto`).
4. Clone the repo:
   ```bash
   git clone https://github.com/legion-works/dormant.git
   cd dormant
   ```

## Local hooks (lefthook)

[lefthook](https://lefthook.dev) runs fast local gates on every commit and push.
Install once after cloning:

```bash
lefthook install
```

- **pre-commit** — `cargo fmt`, `typos`, `taplo fmt --check`, and a staged-only `gitleaks` scan.
- **pre-push** — `cargo clippy -- -D warnings -W clippy::pedantic`, `cargo doc` (the two gates that most often fail CI).

CI remains the authoritative gate — these are local mirrors, not replacements.

## Gate commands

Run the core workspace gates before committing:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings -W clippy::pedantic
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
mdbook build docs
taplo fmt --check
typos
```

The CI jobs in `.github/workflows/ci.yml` are:

- `fmt` — `cargo fmt --all -- --check`
- `webui` — `npm ci`, `npm run lint`, `npm run build`, `npx vitest run`
- `clippy` — workspace/all-targets/all-features with warnings and Clippy pedantic denied
- `test` — `cargo test --workspace --all-features`
- `render` — render-feature builds and tests for `dormant-core`, `dormantd`, and `dormant-render`
- `portability` — `cargo check --workspace` on Windows and macOS
- `deny` and `audit` — dependency policy and RustSec advisories
- `msrv` — `cargo check --workspace` on Rust 1.88
- `mqtt-integration` — live Mosquitto tests, including retained state and availability
- `docs` and `mdbook` — rustdoc with warnings denied, then `mdbook build docs`
- `taplo`, `gitleaks`, and `typos`
- `pr-title` — conventional-commit title validation on pull requests

Run the platform-specific and integration jobs when your change touches those
paths. CI remains the final matrix.

## TDD expectation

Tests are co-located with source: `#[cfg(test)] mod tests { ... }` at the bottom of each `.rs` file, plus integration tests in `tests/` named by the feature they cover (e.g., `tests/rule_grace_period.rs`). Write a failing test first, then implement. The workspace uses `proptest` for property-based testing where input space is large.

## Commits

Conventional commits only: `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`, `ci:`. Scope by crate where useful — `feat(sensors): add LD2412 parser`. These drive the changelog.

## Adding a sensor

Use `crates/dormant-sensors/src/mqtt.rs` as the reference implementation. Steps:

1. **Config variant** — add a new variant to `SensorConfig` in `dormant-core/src/config/schema.rs` with the `type` tag string (e.g., `#[serde(rename = "my-sensor")]`). Inline the common fields (`kind`, `hold_time`, `stale_timeout`).
2. **Module** — create `dormant-sensors/src/my_sensor.rs`. Implement the `SensorSource` trait from `dormant-core/src/traits.rs`.
3. **Registry entry** — add a match arm to the `build` function in `dormant-sensors/src/registry.rs` that constructs your source from its config.
4. **Known-key tree** — add your config keys to the known-key tree in `dormant-core/src/config/mod.rs` so unknown-key detection doesn't reject them.
5. **Validation** — add validation rules in `dormant-core/src/config/validate.rs` (required fields, invalid combinations).
6. **Tests + fixtures** — add a `#[cfg(test)] mod tests` block and a fixture config under `dormant-core/tests/fixtures/config/` that exercises your sensor variant.

## Adding a display controller

Use `crates/dormant-displays/src/command.rs` as the reference implementation. Steps:

1. **Module** — create `dormant-displays/src/my_controller.rs`. Implement the `DisplayController` trait (`name()`, `supported_modes()`, `probe()`, `is_available()`, `blank()`, `wake()`).
2. **Registry entry** — add a match arm to `build_controllers` in `dormant-displays/src/registry.rs`.
3. **Config fields** — add any new fields needed in `DisplayConfig` (schema.rs), with serde defaults.
4. **Rules for `supported_modes()`** — return only modes you have verified work. Do not claim support for a mode that you have not tested on real hardware. A controller that falsely claims `power_off` support can leave a screen on — the worst failure mode.
5. **Fail-safe wake contract** — `wake()` must be idempotent (safe to call on an already-awake display). Wakes retry internally or escalate through the executor's chain; exhausted retries must surface through the existing failure state.
6. **Tests** — mock the controller's I/O surface (process spawn, network, DBus) and test blank/wake round-trips, mode-support filtering, and reachability timeouts.

## PR checklist

- [ ] All gate commands pass locally
- [ ] New public items have doc comments
- [ ] Tests cover the changes (co-located unit tests + integration tests where appropriate)
- [ ] Config keys are in the known-key tree
- [ ] Error codes are literal strings, not `format!`-constructed
- [ ] No hard-coded magic numbers — use constants from `defaults.rs` or config keys
- [ ] Fail-safe presence policy is preserved (unavailable = present, not absent)
- [ ] No telemetry, no phone-home
- [ ] Commit message follows conventional commits format
