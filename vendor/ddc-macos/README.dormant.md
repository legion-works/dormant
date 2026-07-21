# Dormant patch notes: `ddc-macos`

This crate is vendored from upstream, not consumed from crates.io, and pinned to a specific
commit via `[patch.crates-io]` in the workspace root `Cargo.toml`.

- Upstream: <https://github.com/haimgel/ddc-macos-rs>
- Vendored at commit: `232c942fecb89a88cf19c854f12307722aae5318`
- Tracking issue motivating the patch: `haimgel/ddc-macos-rs#8`
- ARM I2C transaction shape ported from m1ddc, reference commit:
  `f95a347285523c9646b9b41b0300c84739e88f00`
  (m1ddc repeats writes but does not expose a raw-VCP surface -- only its write-retry
  transaction *shape* was ported here, not its command vocabulary.)

## Why this fork exists

Upstream hard-links three private, undocumented CoreDisplay symbols
(`IOAVServiceCreateWithService`, `IOAVServiceReadI2C`, `IOAVServiceWriteI2C`) via
`#[link(name = "CoreDisplay", kind = "framework")] extern "C"`. If Apple ever renames or
removes one of these symbols, the entire host process fails to load with a dyld error --
even for callers that never exercise the Apple Silicon ARM code path. `haimgel/ddc-macos-rs#8`
tracks exactly this class of failure.

This patch replaces the hard link with a runtime-resolved symbol table
(`arm::CoreDisplaySymbols`), loaded once via `dlopen`/`dlsym` against
`/System/Library/Frameworks/CoreDisplay.framework/CoreDisplay` and cached in a `OnceLock`. A
symbol that fails to resolve now surfaces as a typed `Error::MissingCoreDisplaySymbol`, and the
process still loads and runs.

It also replaces the ARM write path with the proven m1ddc transaction shape: sleep 10ms, write
the exact encoded packet, repeat once (two pre-delayed writes total, no additional
controller-level retry -- retry policy belongs to the executor above this crate). A read
transaction waits the caller-provided response delay and performs exactly one read. The
transaction stops immediately on the first nonzero `OSStatus`.

The injectable `arm::ArmI2c` trait separates this transaction driver (`arm::execute_with`)
from the real CoreDisplay I/O (`arm::CoreDisplayIo`), so the transaction shape and symbol
resolution can be covered by fork-local unit tests without real Apple Silicon hardware. Those
tests are macOS-target code and cannot run in a Linux CI/dev sandbox; they run in the project's
macOS PR CI lane.

## What did NOT change

- The upstream public types (`Monitor`, `Error`, etc.) and the `DdcCommandRaw` implementation
  are preserved byte-for-byte in shape. `ddc-hi` does not require any dormant-specific API.
- `Monitor::execute_raw` remains generic over arbitrary VCP opcodes; there is no
  dormant-specific "read usage hours" convenience API in this fork.
- The upstream package version (`0.2.2`) and MIT `LICENSE` are unchanged. See
  `[package.metadata.dormant]` in `Cargo.toml` for patch provenance metadata.

On 2026-07-21, the fork's generic `DdcCommandRaw` surface was proven for the VCP 0x60
input-source GET request. The fork-local packet test pins `[0x51, 0x82, 0x01, 0x60, 0xDC]`,
including its DDC/CI checksum, and the ignored macOS hardware test reads and prints the current
input source. No `arm.rs` change was necessary: the existing generic transport accepts the raw
opcode without a controller-specific branch.

## Files changed from upstream

- `src/arm.rs`: `CoreDisplaySymbols` + `SymbolLoader`/`ArmI2c` seam, m1ddc transaction shape,
  co-located fork tests.
- `src/monitor.rs`: test-only packet encoder accessor (`Monitor::encode_command_for_test`),
  co-located fork test.
- `src/error.rs`: added `Clone` derive, needed for the `OnceLock<Result<_, Error>>` symbol
  cache; added two new typed variants (`MissingCoreDisplaySymbol`,
  `CoreDisplayFrameworkUnavailable`) for the dlopen/dlsym seam.
- `Cargo.toml`: added `libc` (for `dlopen`/`dlsym`), added `[package.metadata.dormant]`,
  dropped unused `edid-rs`/`nom` dev-dependencies (this vendor tree does not include upstream's
  `tests/`/`examples/`).
