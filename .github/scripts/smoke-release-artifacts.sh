#!/usr/bin/env bash
# Smoke-test packaged Linux release artifacts (#38).
#
# Verifies that the cargo-dist-produced `dormantd` / `dormantctl` archives for
# a given target triple actually contain runnable binaries, and that the
# matching shell installer artifact exists, *before* the `host` job is allowed
# to publish the GitHub release. This does not start the daemon or exercise
# any hardware; it only proves archive integrity and dynamic-link startup
# (`--version`, `--help`, `--validate-only`).
#
# Artifact selection is entirely manifest-driven (dist-manifest.json /
# `dist plan --output-format=json`'s `.artifacts` map) — never `find | head`
# guesswork — so the same archive dist just built is the one we smoke-test.
#
# Usage:
#   smoke-release-artifacts.sh <manifest.json> <artifacts-dir> [target-triple] [expected-version]
#
# - <manifest.json>: a cargo-dist manifest (dist-manifest.json or the `plan`
#   job's JSON `.val` output) describing the artifacts that were/will be built.
# - <artifacts-dir>: directory containing the downloaded artifact files named
#   exactly as they appear in the manifest (flat, as `actions/download-artifact`
#   with `merge-multiple: true` produces).
# - [target-triple]: defaults to x86_64-unknown-linux-gnu.
# - [expected-version]: defaults to the manifest's announcement_tag with any
#   leading "v" stripped.
#
# Exit codes: 0 on success; 1 with a literal `missing packaged binary: <name>`
# or `missing packaged installer: <name>` message identifying the first thing
# that could not be found/run.

set -euo pipefail

die() {
    echo "$1" >&2
    exit 1
}

if [ "$#" -lt 2 ]; then
    die "usage: smoke-release-artifacts.sh <manifest.json> <artifacts-dir> [target-triple] [expected-version]"
fi

MANIFEST="$1"
ARTIFACTS_DIR="$2"
TARGET_TRIPLE="${3:-x86_64-unknown-linux-gnu}"

command -v jq >/dev/null 2>&1 || die "smoke-release-artifacts.sh requires jq"

[ -f "$MANIFEST" ] || die "missing dist manifest: $MANIFEST"
[ -d "$ARTIFACTS_DIR" ] || die "missing artifacts directory: $ARTIFACTS_DIR"

EXPECTED_VERSION="${4:-}"
if [ -z "$EXPECTED_VERSION" ]; then
    EXPECTED_VERSION="$(jq -r '.announcement_tag // empty' "$MANIFEST" | sed 's/^v//')"
fi
[ -n "$EXPECTED_VERSION" ] || die "manifest has no announcement_tag and no expected-version was given"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/dormant-release-smoke.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT

PREFIX="$WORKDIR/prefix"
mkdir -p "$PREFIX/bin"

# Resolve, for a given app, the single executable-zip artifact name in the
# manifest that (a) targets $TARGET_TRIPLE and (b) bundles an executable
# asset named exactly $app. Deterministic: a manifest that produced two or
# more matches for the same app+triple is itself a manifest bug we want to
# surface, not silently resolve via `head -1`.
resolve_archive_name() {
    app="$1"
    jq -r --arg app "$app" --arg triple "$TARGET_TRIPLE" '
        .artifacts // {}
        | to_entries[]
        | select(.value.kind == "executable-zip")
        | select((.value.target_triples // []) | index($triple))
        | select((.value.assets // []) | map(select(.kind == "executable") | .name) | index($app))
        | .key
    ' "$MANIFEST"
}

# Resolve, for a given executable-zip artifact name, the in-archive path of
# its named executable asset (per the manifest, not a guess about tar layout).
resolve_archive_exe_path() {
    archive="$1"
    app="$2"
    jq -r --arg archive "$archive" --arg app "$app" '
        .artifacts[$archive].assets // []
        | map(select(.kind == "executable" and .name == $app))
        | .[0].path // empty
    ' "$MANIFEST"
}

stage_binary() {
    app="$1"

    matches="$(resolve_archive_name "$app")"
    match_count="$(printf '%s\n' "$matches" | grep -c . || true)"

    if [ "$match_count" -eq 0 ]; then
        die "missing packaged binary: $app"
    fi
    if [ "$match_count" -gt 1 ]; then
        die "ambiguous packaged binary: $app matches multiple manifest artifacts for $TARGET_TRIPLE"
    fi

    archive_name="$matches"
    archive_path="$ARTIFACTS_DIR/$archive_name"
    [ -f "$archive_path" ] || die "missing packaged binary: $app"

    exe_in_archive="$(resolve_archive_exe_path "$archive_name" "$app")"
    [ -n "$exe_in_archive" ] || die "missing packaged binary: $app"

    extract_dir="$WORKDIR/extract/$app"
    mkdir -p "$extract_dir"
    tar -xf "$archive_path" -C "$extract_dir" 2>/dev/null || die "missing packaged binary: $app"

    # cargo-dist nests the binary under a "<app>-<triple>/" prefix directory
    # inside the tarball, but the manifest's asset `.path` reports only the
    # bare in-archive basename (e.g. "dormantd", not
    # "dormantd-x86_64-unknown-linux-gnu/dormantd"). Trusting `.path` as an
    # extract-relative path therefore misses the real file. Prefer the manifest
    # path if it happens to land (future-proof against a flat layout), else
    # locate the executable by its basename anywhere under the extract dir.
    extracted_exe="$extract_dir/$exe_in_archive"
    if [ ! -f "$extracted_exe" ]; then
        extracted_exe="$(find "$extract_dir" -type f -name "$(basename "$exe_in_archive")" 2>/dev/null | head -n1)"
    fi
    [ -n "$extracted_exe" ] && [ -f "$extracted_exe" ] || die "missing packaged binary: $app"

    install -m 755 "$extracted_exe" "$PREFIX/bin/$app"
}

# Order matters: dormantd is checked first so an empty fixture's first
# failure is deterministically "missing packaged binary: dormantd".
#
# We deliberately do NOT smoke the shell installer here: it is a cargo-dist
# *global* artifact (built by build-global-artifacts), while this job depends
# only on `plan` and receives the per-target build-local archives — the
# installer file is not present in ARTIFACTS_DIR. It is also generated
# boilerplate that merely curls the published release, so its integrity is
# cargo-dist's concern, not ours; the value of this gate is proving the
# packaged BINARIES extract and run before `host` publishes.
stage_binary "dormantd"
stage_binary "dormantctl"

CONFIG_FILE="$WORKDIR/config.toml"
CREDENTIALS_FILE="$WORKDIR/credentials.toml" # intentionally never created:
# dormant_core::config::load_credentials treats a missing file as empty
# credentials, so this also proves the packaged binary doesn't hard-require
# a credentials file to validate a minimal config.
printf 'config_version = 1\n' > "$CONFIG_FILE"

assert_version() {
    bin="$1"
    out="$("$PREFIX/bin/$bin" --version)"
    case "$out" in
        "$bin "*"$EXPECTED_VERSION"*) ;;
        *)
            die "version mismatch for $bin: expected $EXPECTED_VERSION, got: $out"
            ;;
    esac
}

assert_version "dormantd"
assert_version "dormantctl"

"$PREFIX/bin/dormantctl" --help >/dev/null

"$PREFIX/bin/dormantd" \
    --config "$CONFIG_FILE" \
    --credentials "$CREDENTIALS_FILE" \
    --validate-only

echo "release-artifact-smoke: dormantd and dormantctl ($TARGET_TRIPLE, $EXPECTED_VERSION) OK"
