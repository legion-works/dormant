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

- **pre-commit** — formatting, spelling, staged gitleaks, and the staged test-timing policy.
- **pre-push** — changed Rust/workspace inputs run Clippy, rustdoc, and nextest plus doctests. Web UI, mdBook, dependency-policy, and MSRV changes run their matching gates. This costs more than the old hook; it is intended to catch CI failures before the push.

CI remains the authoritative gate — these are local mirrors, not replacements.

## Gate commands

Every local and CI gate enters through `scripts/gates/`; do not copy command bodies into YAML. Run the entry points directly when diagnosing one gate:

```bash
bash scripts/gates/fmt.sh
bash scripts/gates/clippy.sh
bash scripts/gates/rust-tests.sh      # nextest plus doctests
bash scripts/gates/rustdoc.sh
bash scripts/gates/webui.sh
bash scripts/gates/deny.sh
bash scripts/gates/msrv.sh
bash scripts/gates/mdbook.sh
bash scripts/gates/taplo.sh
bash scripts/gates/typos.sh
bash scripts/gates/gitleaks.sh staged
```

`rust-tests.sh` uses the `ci` nextest profile and then runs `cargo test --doc`; nextest does not run doctests. Missing optional tools print their install command and fail rather than silently skipping a gate.

Do not rerun a failed test until it turns green. Record flakes in `.github/flake-ledger.toml` with the required evidence, fix the cause, and use the ledger policy instead of rerun roulette.

The CI jobs in `.github/workflows/ci.yml` are:

- `fmt` — `cargo fmt --all -- --check`
- `webui` — `npm ci`, `npm run lint`, `npm run build`, `npx vitest run`
- `clippy` — workspace/all-targets/all-features with warnings and Clippy pedantic denied
- `test` — nextest under the `ci` profile plus doctests
- `render` — render-feature builds and tests for `dormant-core`, `dormantd`, and `dormant-render`
- `windows-portability` — `cargo check --workspace` on Windows
- `macos-test` — workspace tests plus the vendored DDC transport tests on macOS
- `macos-msrv` — Rust 1.88 workspace and vendored DDC transport checks on macOS
- `deny` — dependency policy and RustSec advisories
- `msrv` — `cargo check --workspace` on Rust 1.88
- `mqtt-integration` — live Mosquitto tests, including retained state and availability
- `docs` and `mdbook` — rustdoc with warnings denied, then `mdbook build docs`
- `taplo`, `gitleaks`, and `typos`
- `pr-title` — conventional-commit title validation on pull requests

Run the platform-specific and integration jobs when your change touches those
paths. CI remains the final matrix.

## Known dependency warnings

`cargo check`/`cargo build`/`cargo test` print a future-incompatibility
warning for `nom v3.2.1`:

```
warning: the following packages contain code that will be rejected by a future version of Rust: nom v3.2.1
note: to see what the problems were, use the option `--future-incompat-report`, or run `cargo report future-incompatibilities --id <id>`
```

**Dependency chain** (`cargo tree -i nom@3.2.1`):

```
nom v3.2.1
├── edid v0.3.0
│   └── ddc-hi v0.4.1
│       └── dormant-displays v0.1.0
├── mccs-caps v0.1.3
│   └── ddc-hi v0.4.1 (*)
└── mccs-db v0.1.3
    └── ddc-hi v0.4.1 (*)
```

`ddc-hi` is `dormant-displays`'s DDC/CI backend (`vcp_ops.rs`) and is
hardware-verified against real monitors (see `docs/research/`); `0.4.1` is
still its latest published release, so there is no newer `ddc-hi` to move to.
The lint itself (`trailing semicolon in macro used in expression position`,
rust-lang/rust#79813) is cosmetic — old macro-generated code inside `nom`
3.2.1, not a soundness issue in `dormant`. Forking `ddc-hi`/`nom` or adding a
`[patch.crates-io]` override to silence a cosmetic warning is disproportionate
next to the risk of touching a verified DDC/CI code path, so the warning is
recorded and left visible rather than papered over.

We checked whether Cargo (1.96-era) supports acknowledging or suppressing
this warning for `nom` alone: `cargo report future-incompatibilities --id
<id> -p nom@3.2.1` only filters which package's *report detail* is printed
for a report that already exists — it doesn't stop the warning from
appearing on the next build. The only build-wide switch is the
`[future-incompat-report] frequency` config key (`always`/`never`), which is
all-or-nothing across every dependency, not package-scoped, so it is
intentionally not used here. There is no `[patch.crates-io]` fork in this
repo for `nom`/`ddc-hi`.

**Chase condition** — re-evaluate when either happens:

- `ddc-hi` releases a version above `0.4.1` (check for a `nom` bump), or
- a future `rustc` turns this lint into a hard error (the build will fail
  instead of warning, forcing the issue).

**Maintenance check:**

```bash
cargo tree -i nom@3.2.1
```

If this command errors (no matching package), `nom 3.2.1` is gone from the
tree and this note can be deleted.

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
