#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf 'usage: %s --platform <Linux|Darwin> --stress-count <positive integer>\n' "$0" >&2
  exit 2
}

platform=''
stress_count=''
while (($#)); do
  case "$1" in
    --platform)
      (($# >= 2)) || usage
      platform="$2"
      shift 2
      ;;
    --stress-count)
      (($# >= 2)) || usage
      stress_count="$2"
      shift 2
      ;;
    *)
      usage
      ;;
  esac
done

case "$platform" in
  Linux|Darwin) ;;
  *) usage ;;
esac

[[ "$stress_count" =~ ^[1-9][0-9]*$ ]] || usage

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
artifact_root="$target_dir/hermeticity-stress"
filter='test(concurrent_apps_keep_wear_paths_and_observations_isolated)'

cd "$repo_root"
mkdir -p "$artifact_root/process-one" "$artifact_root/process-two"

printf 'prebuilding daemon_smoke test binary for %s\n' "$platform"
PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo nextest run --no-run -p dormantd --test daemon_smoke

selected="$(PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo nextest list -p dormantd --test daemon_smoke -E "$filter")"
printf '%s\n' "$selected"
if [[ -z "$selected" ]] || [[ "$(printf '%s\n' "$selected" | wc -l)" -lt 1 ]]; then
  printf 'hermeticity filter selected no tests\n' >&2
  exit 1
fi

write_config() {
  local process_name="$1"
  local config_path="$artifact_root/$process_name/nextest.toml"
  local junit_path="$artifact_root/$process_name/junit.xml"

  cat > "$config_path" <<EOF
[profile.default]
retries = 0
flaky-result = "fail"
fail-fast = false

[profile.default.junit]
path = "$junit_path"
EOF
}

write_config process-one
write_config process-two

# TempDir derives each test's state, config, credentials, socket, and marker roots from TMPDIR.
run_process() {
  local process_name="$1"
  local process_root="$artifact_root/$process_name"
  local process_tmp="$process_root/tmp"
  local process_log="$process_root/nextest.log"

  mkdir -p "$process_tmp"
  (
    export TMPDIR="$process_tmp"
    export TEMP="$process_tmp"
    export TMP="$process_tmp"
    export CARGO_TARGET_DIR="$target_dir"
    PKG_CONFIG_PATH=/usr/lib/pkgconfig cargo nextest run \
      --config-file "$process_root/nextest.toml" \
      -p dormantd --test daemon_smoke \
      --stress-count "$stress_count" --retries 0 --flaky-result fail \
      -E "$filter"
  ) > "$process_log" 2>&1 &
  process_pid="$!"
}

run_process process-one
pid_one="$process_pid"
run_process process-two
pid_two="$process_pid"

cleanup() {
  kill "$pid_one" "$pid_two" 2>/dev/null || true
}
trap cleanup INT TERM

status=0
if ! wait "$pid_one"; then
  status=1
fi
if ! wait "$pid_two"; then
  status=1
fi
trap - INT TERM

if ((status)); then
  printf 'one or more concurrent nextest processes failed; logs are under %s\n' "$artifact_root" >&2
  exit 1
fi

printf 'both concurrent nextest processes passed; artifacts are under %s\n' "$artifact_root"
