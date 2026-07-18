# cargo-nextest CI policy spike

Date: 2026-07-18  
Host: `x86_64-unknown-linux-gnu`  
Purpose: verify the pinned CI tool floor, retry/JUnit semantics, explicit
configuration, and stress-test loop behavior before changing the workflows.

## Pinned tools and workspace gates

Installed with `--locked` at the exact requested versions:

```text
cargo install cargo-nextest --version 0.9.131 --locked
cargo install mdbook --version 0.4.52 --locked
cargo install cargo-deny --version 0.18.4 --locked
cargo install cargo-audit --version 0.21.2 --locked
cargo install taplo-cli --version 0.10.0 --locked
```

Version output:

```text
cargo-nextest 0.9.131 (af40d2e87 2026-03-18)
mdbook v0.4.52
cargo-deny 0.18.4
cargo-audit-audit 0.21.2
taplo 0.10.0
```

Commands were run from the repository root with
`PKG_CONFIG_PATH=/usr/lib/pkgconfig` for workspace commands:

| Command | Exit code | Result |
|---|---:|---|
| `mdbook build docs` | 0 | pass |
| `cargo deny check` | 1 | blocked by the installed advisory database containing CVSS 4.0 records, unsupported by cargo-deny 0.18.4 |
| `cargo audit` | 1 | blocked by the fetched advisory database containing CVSS 4.0 records, unsupported by cargo-audit 0.21.2 |
| `taplo fmt --check` | 0 | pass |

The reproducible advisory-database error was:

```text
unsupported CVSS version: 4.0
```

This is an advisory database/parser compatibility failure, not a workspace
dependency finding. No tool version was substituted.

## Disposable fixture

The fixture was created under `/tmp/tmp.SrgNWzZGju` and was not committed.
Its tests were `pass_once`, `fail_once`, `always_fails`, and `counter`.
`fail_once` used `OpenOptions::new().write(true).create_new(true).open(state)`
against `DORMANT_SPIKE_STATE`; `counter` used
`OpenOptions::new().create(true).append(true).open(path)` and one
newline-terminated `write_all(b"run\\n")` call against
`DORMANT_SPIKE_COUNT`.

The effective `.config/nextest.toml` was:

```toml
[profile.ci]
retries = 2
flaky-result = "fail"
fail-fast = false

[profile.ci.junit]
path = "junit.xml"
```

nextest resolves the JUnit path relative to its profile report directory, so
this produces the required `target/nextest/ci/junit.xml`.

## Retry and JUnit evidence

Run from a clean `DORMANT_SPIKE_STATE`:

```bash
set +e
cargo nextest run --profile ci -E 'test(fail_once)'
status=$?
set -e
test "$status" -ne 0
```

Observed: attempt 1 failed, attempt 2 passed, and the process exited `100`.
This is the required **non-zero after retry passed** result from
`flaky-result = "fail"`; nextest printed `test configured to fail if flaky`.

`cargo nextest run --config-file "$TMPDIR/nextest-spike.toml" --profile ci
-E 'test(fail_once)'` also selected the fixture and emitted the configured
JUnit file; its exit code was `100` for the same intentional flaky failure.

Python `xml.etree.ElementTree` parsed the actual 0.9.131 output. The minimal
flaky shape, verbatim apart from volatile UUID, timestamps, and process ID,
was:

```xml
<testsuites name="nextest-run" tests="1" failures="1" errors="0" uuid="..." timestamp="..." time="...">
  <testsuite name="nextest-spike::bin/nextest-spike" tests="1" disabled="0" errors="0" failures="1">
    <testcase name="fail_once" classname="nextest-spike::bin/nextest-spike" timestamp="..." time="...">
      <failure message="test passed on attempt 2/3 but is configured to fail when flaky" type="flaky failure" />
      <flakyFailure timestamp="..." time="..." message="thread 'fail_once' (...) panicked at src/main.rs:13:9" type="test failure with exit code 101">...
      </flakyFailure>
    </testcase>
  </testsuite>
</testsuites>
```

For `always_fails`, `cargo nextest run --config-file
"$TMPDIR/nextest-spike.toml" --profile ci -E 'test(always_fails)'` exited
`100`. Its distinct permanent-failure shape was:

```xml
<testcase name="always_fails" classname="nextest-spike::bin/nextest-spike" timestamp="..." time="...">
  <failure message="thread 'always_fails' (...) panicked at src/main.rs:19:5" type="test failure with exit code 101">...</failure>
  <rerunFailure timestamp="..." time="..." message="thread 'always_fails' (...) panicked at src/main.rs:19:5" type="test failure with exit code 101">...</rerunFailure>
  <rerunFailure timestamp="..." time="..." message="thread 'always_fails' (...) panicked at src/main.rs:19:5" type="test failure with exit code 101">...</rerunFailure>
</testcase>
```

The parser must identify flaky tests through `failure[type="flaky failure"]`
and `flakyFailure`, while permanent failures use `failure` plus zero or more
`rerunFailure` elements and no `flakyFailure`.

## Stress evidence

```bash
cargo nextest run --stress-count 3 --retries 0 --flaky-result fail -E 'test(counter)'
test "$(wc -l < "$DORMANT_SPIKE_COUNT")" -eq 3
```

The command reported three passed stress iterations, and the append-only
counter contained exactly `3` newline-terminated records. The `--retries 0`
override prevents profile retries from multiplying the count.

## macos-14 availability

Probed 2026-07-18 via disposable draft PR #103 (workflow `macos-14-probe.yml`,
closed unmerged): `runs-on: macos-14` resolved and ran green.

- macos-14 availability: CONFIRMED — job completed successfully
- `sw_vers` ProductVersion: 14.8.7 (arm64)
- Runner image: `macos-14-arm64/20260629.0180`
- Image readme: https://github.com/actions/runner-images/blob/macos-14-arm64/20260629.0180/images/macos/macos-14-arm64-Readme.md

T8/T18 may pin `macos-14`. Caveat recorded: macos-14 runners are Apple Silicon
(arm64) — same as the current `macos-latest` lanes, so no target-triple change.
