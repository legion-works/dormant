#!/usr/bin/env bash
# Build and install dormantd/dormantctl/dormant-tray to ~/.local/bin.
#
# This exists because a hand-rolled `cargo build --release` has shipped a
# daemon serving a blank web UI three times. `rust-embed` bakes
# crates/dormant-web/webui/dist/ into the binary at compile time, and the
# checked-in dist/index.html is a placeholder — so a release build without a
# prior `npm run build` silently embeds an empty page. dormant-web's build.rs
# now refuses that combination outright; this script is the path that never
# reaches it.
#
# Usage:
#   bash scripts/deploy-local.sh              # build + install + restart
#   bash scripts/deploy-local.sh --no-restart # build + install only
#   bash scripts/deploy-local.sh --dry-run    # build + verify, install nothing

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WEBUI_DIR="$REPO_ROOT/crates/dormant-web/webui"
INSTALL_DIR="${DORMANT_INSTALL_DIR:-$HOME/.local/bin}"
SERVICE="app-dormant.service"
TRAY_SERVICE="dormant-tray.service"
SKIPPED=""

RESTART=1
DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --no-restart) RESTART=0 ;;
    --dry-run) DRY_RUN=1; RESTART=0 ;;
    -h|--help) sed -n '1,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

# libudev is not always discoverable without this, depending on layout.
export PKG_CONFIG_PATH="${PKG_CONFIG_PATH:-/usr/lib/pkgconfig}"

echo "==> Building web UI (rust-embed reads this at compile time)"
cd "$WEBUI_DIR"
npm ci --silent
npm run build --silent

# Fail here rather than embedding a placeholder. build.rs enforces the same
# invariant, but failing before a long cargo build is a better experience.
if grep -q 'PLACEHOLDER' "$WEBUI_DIR/dist/index.html"; then
  echo "ERROR: dist/index.html is still the placeholder after npm run build." >&2
  echo "       The Vite build did not write its output where rust-embed reads it." >&2
  exit 1
fi
echo "    dist/index.html: $(wc -c < "$WEBUI_DIR/dist/index.html") bytes (real bundle)"

echo "==> Building binaries"
cd "$REPO_ROOT"

# Ask cargo which files it produced instead of reconstructing the path.
#
# `--message-format=json` emits a `compiler-artifact` record carrying the
# absolute `executable` path for every binary built, including when the artifact
# was already up to date -- so a no-op rebuild still reports. That is
# authoritative: it settles the target-dir question (CARGO_TARGET_DIR redirects
# the build without changing $REPO_ROOT, which shipped stale binaries through a
# whole debugging session) and the staleness question together, with one
# mechanism and no heuristics.
build_exes() {
  cargo build --release --message-format=json "$@" \
    | python3 -c '
import json, sys
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") == "compiler-artifact" and m.get("executable"):
        print(m["executable"])
'
}

# dormantd needs its features named explicitly. Folding it into a multi-package
# build silently drops them, which is its own recurring failure.
BUILT_EXES="$(build_exes -p dormantd --features web-ui,render)"
BUILT_EXES="$BUILT_EXES
$(build_exes -p dormantctl -p dormant-tray)"

# Look each binary up in what cargo reported, so the file verified below and the
# file installed later are the same one cargo just wrote.
exe_path() {
  printf '%s\n' "$BUILT_EXES" | grep -E "/$1\$" | head -1
}

for required in dormantd dormantctl dormant-tray; do
  if [ -z "$(exe_path "$required")" ]; then
    echo "ERROR: cargo reported no executable for $required." >&2
    echo "       Built artifacts were:" >&2
    printf '%s\n' "$BUILT_EXES" | sed 's/^/         /' >&2
    exit 1
  fi
done

echo "    target dir: $(dirname "$(exe_path dormantd)")"

echo "==> Verifying the built daemon"
BIN="$(exe_path dormantd)"
[ -x "$BIN" ] || { echo "ERROR: $BIN missing" >&2; exit 1; }

# Dump once and grep the file. Do NOT pipe `strings` into `grep -q` here:
# under `set -o pipefail`, grep -q exits on the first match, strings takes
# SIGPIPE, and the pipeline reports failure *because the match succeeded*.
# That inverts every check — the placeholder test would silently never fire.
SYMS="$(mktemp)"
trap 'rm -f "$SYMS"' EXIT
strings "$BIN" > "$SYMS"

# Catches a dropped --features flag.
if ! grep -q 'web_listening' "$SYMS"; then
  echo "ERROR: dormantd was built without the web-ui feature." >&2
  exit 1
fi

# Catches an embedded placeholder, asserted against the artifact itself
# rather than the source tree it was built from.
if grep -q 'PLACEHOLDER: replaced by the real Vite build' "$SYMS"; then
  echo "ERROR: the built dormantd has the placeholder SPA embedded." >&2
  exit 1
fi

# Positive assertion: the real bundle's hashed script tag is in the binary.
if ! grep -q '/assets/index-.*\.js' "$SYMS"; then
  echo "ERROR: no hashed asset reference found in the binary — SPA not embedded." >&2
  exit 1
fi
echo "    web-ui feature: present"
echo "    embedded SPA:   real bundle"

if [ "$DRY_RUN" -eq 1 ]; then
  echo "==> --dry-run: built and verified, installing nothing"
  exit 0
fi

echo "==> Installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
BACKUP_DIR="${TMPDIR:-/tmp}/dormant-deploy-backup-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$BACKUP_DIR"

if [ "$RESTART" -eq 1 ] && systemctl --user is-active --quiet "$SERVICE"; then
  echo "    stopping $SERVICE"
  systemctl --user stop "$SERVICE"
  STOPPED=1
else
  STOPPED=0
fi

# The tray holds its own binary open, so cp fails with "Text file busy" while it
# runs. Stop it the same way -- an unhandled failure here aborts the deploy with
# the DAEMON ALREADY STOPPED, which is how a routine deploy once left the
# machine with no running dormantd.
if [ "$RESTART" -eq 1 ] && systemctl --user is-active --quiet "$TRAY_SERVICE"; then
  echo "    stopping $TRAY_SERVICE"
  systemctl --user stop "$TRAY_SERVICE"
  TRAY_STOPPED=1
else
  TRAY_STOPPED=0
fi

for bin in dormantd dormantctl dormant-tray; do
  if [ -f "$INSTALL_DIR/$bin" ]; then
    cp "$INSTALL_DIR/$bin" "$BACKUP_DIR/$bin"
  fi
  # A still-running binary yields "Text file busy". Both services are stopped
  # above; anything else holding one open (a hand-started tray) is reported and
  # skipped rather than aborting mid-install with the daemon down.
  if cp "$(exe_path "$bin")" "$INSTALL_DIR/$bin" 2>/dev/null; then
    echo "    installed $bin"
  else
    echo "    WARN: could not replace $bin (still running?) -- left as-is" >&2
    SKIPPED="$SKIPPED $bin"
  fi
done
echo "    previous binaries backed up to $BACKUP_DIR"

if [ "$STOPPED" -eq 1 ]; then
  echo "    starting $SERVICE"
  systemctl --user start "$SERVICE"
  sleep 2
  if systemctl --user is-active --quiet "$SERVICE"; then
    echo "    $SERVICE is active"
  else
    echo "ERROR: $SERVICE failed to start. Roll back with:" >&2
    echo "  systemctl --user stop $SERVICE && cp $BACKUP_DIR/dormantd $INSTALL_DIR/dormantd && systemctl --user start $SERVICE" >&2
    exit 1
  fi
fi

if [ "$TRAY_STOPPED" -eq 1 ]; then
  echo "    starting $TRAY_SERVICE"
  systemctl --user start "$TRAY_SERVICE"
fi

if [ -n "$SKIPPED" ]; then
  echo "WARN: not replaced:$SKIPPED (still running)" >&2
fi

echo "==> Done. Roll back with:"
echo "    cp $BACKUP_DIR/<binary> $INSTALL_DIR/"
