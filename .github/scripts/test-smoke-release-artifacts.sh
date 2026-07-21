#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/dormant-smoke-shape.XXXXXX")"
trap 'rm -rf "$ROOT"' EXIT
ARTIFACTS="$ROOT/artifacts"
PAYLOADS="$ROOT/payloads"
FAKE_BIN="$ROOT/fake-bin"
TRIPLE="aarch64-apple-darwin"
VERSION="0.0.0"
mkdir -p "$ARTIFACTS" "$PAYLOADS" "$FAKE_BIN"

cat > "$ROOT/fake-app" <<'APP'
#!/usr/bin/env bash
set -euo pipefail
name="$(basename "$0")"
if [ "${1:-}" = "--version" ]; then
    printf '%s 0.0.0\n' "$name"
fi
exit 0
APP
chmod +x "$ROOT/fake-app"

cat > "$FAKE_BIN/plutil" <<'PLUTIL'
#!/usr/bin/env bash
set -euo pipefail
[ "$1" = "-lint" ]
python3 - "$2" <<'PY'
import plistlib
import sys
with open(sys.argv[1], 'rb') as handle:
    plistlib.load(handle)
PY
PLUTIL
chmod +x "$FAKE_BIN/plutil"

for app in dormantd dormantctl dormant-tray; do
    prefix="$PAYLOADS/$app-$TRIPLE"
    mkdir -p "$prefix"
    cp "$ROOT/fake-app" "$prefix/$app"
done
cp crates/dormantd/share/com.legionworks.dormant.plist \
    "$PAYLOADS/dormantd-$TRIPLE/com.legionworks.dormant.plist"
cp crates/dormantd/systemd/dormant.service \
    "$PAYLOADS/dormantd-$TRIPLE/dormant.service"
cp crates/dormant-tray/share/com.legionworks.dormant-tray.plist \
    "$PAYLOADS/dormant-tray-$TRIPLE/com.legionworks.dormant-tray.plist"

for app in dormantd dormantctl dormant-tray; do
    tar -cJf "$ARTIFACTS/$app-$TRIPLE.tar.xz" \
        -C "$PAYLOADS" "$app-$TRIPLE"
done

cat > "$ROOT/manifest.json" <<JSON
{
  "announcement_tag": "v$VERSION",
  "artifacts": {
    "dormantd-$TRIPLE.tar.xz": {
      "kind": "executable-zip",
      "target_triples": ["$TRIPLE"],
      "assets": [
        {"kind": "executable", "name": "dormantd", "path": "dormantd"},
        {"kind": "extra", "name": "com.legionworks.dormant.plist", "path": "com.legionworks.dormant.plist"},
        {"kind": "extra", "name": "dormant.service", "path": "dormant.service"}
      ]
    },
    "dormantctl-$TRIPLE.tar.xz": {
      "kind": "executable-zip",
      "target_triples": ["$TRIPLE"],
      "assets": [
        {"kind": "executable", "name": "dormantctl", "path": "dormantctl"}
      ]
    },
    "dormant-tray-$TRIPLE.tar.xz": {
      "kind": "executable-zip",
      "target_triples": ["$TRIPLE"],
      "assets": [
        {"kind": "executable", "name": "dormant-tray", "path": "dormant-tray"},
        {"kind": "extra", "name": "com.legionworks.dormant-tray.plist", "path": "com.legionworks.dormant-tray.plist"}
      ]
    }
  }
}
JSON

PATH="$FAKE_BIN:$PATH" .github/scripts/smoke-release-artifacts.sh \
    "$ROOT/manifest.json" "$ARTIFACTS" "$TRIPLE" "$VERSION" > "$ROOT/pass.out"
grep -F 'dormant-tray' "$ROOT/pass.out"
grep -F 'com.legionworks.dormant-tray.plist' "$ROOT/pass.out"

rm "$PAYLOADS/dormant-tray-$TRIPLE/com.legionworks.dormant-tray.plist"
tar -cJf "$ARTIFACTS/dormant-tray-$TRIPLE.tar.xz" \
    -C "$PAYLOADS" "dormant-tray-$TRIPLE"
if PATH="$FAKE_BIN:$PATH" .github/scripts/smoke-release-artifacts.sh \
    "$ROOT/manifest.json" "$ARTIFACTS" "$TRIPLE" "$VERSION" \
    > "$ROOT/fail.out" 2> "$ROOT/fail.err"; then
    echo 'Apple smoke unexpectedly accepted a missing tray plist' >&2
    exit 1
fi
grep -F 'missing packaged file: com.legionworks.dormant-tray.plist' "$ROOT/fail.err"
