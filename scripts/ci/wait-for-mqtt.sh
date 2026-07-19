#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 || ! $2 =~ ^[0-9]+$ || ! $3 =~ ^[1-9][0-9]*$ ]]; then
  printf 'Usage: %s <host> <port> <attempts>\n' "$0" >&2
  exit 2
fi

host=$1
port=$2
attempts=$3
topic="dormant/ci-readiness/${RANDOM}-${RANDOM}-$$"
payload="ready-${RANDOM}-${RANDOM}-$$"
output=$(mktemp)
subscriber=''

cleanup() {
  if [[ -n $subscriber ]]; then
    kill "$subscriber" 2>/dev/null || true
  fi
  rm -f "$output"
}
trap cleanup EXIT

for attempt in $(seq 1 "$attempts"); do
  mosquitto_sub -h "$host" -p "$port" -q 1 -t "$topic" -C 1 -W 2 >"$output" &
  subscriber=$!
  sleep 0.1

  if mosquitto_pub -h "$host" -p "$port" -q 1 -t "$topic" -m "$payload" \
    && wait "$subscriber" \
    && [[ $(<"$output") == "$payload" ]]; then
    printf 'MQTT readiness round-trip succeeded on attempt %s\n' "$attempt"
    exit 0
  fi

  kill "$subscriber" 2>/dev/null || true
  wait "$subscriber" 2>/dev/null || true
  subscriber=''
  : >"$output"
  sleep 1
done

printf 'MQTT readiness round-trip failed after %s attempts\n' "$attempts" >&2
exit 1
