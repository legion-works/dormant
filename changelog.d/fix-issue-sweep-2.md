---
kind: fix
surfaces: [cli, web]
issue: 204, 205
---
`dormantd` and `dormantctl` now reject malformed MQTT broker URLs (empty host, non-numeric port suffix like `host:1883x`, unclosed bracket) instead of silently connecting to a garbage host, and now resolve bare IPv6 `tcp://::1` to host `::1` and bracketed IPv6 `tcp://[::1]:1884` correctly. A retained `online` availability signal is now bounded by the sensor's `stale_timeout` — an `online` sensor that goes silent past `stale_timeout` is demoted to `Unavailable` so a dead broker cannot preserve stale presence forever (fail-safe-present zone policy is preserved).

Detail: `parse_broker_url` now returns `Result<(&str, u16), BrokerUrlError>` with `EmptyHost` and `MalformedPort` variants; `availability_online` is now a `HashMap<SensorId, tokio::time::Instant>` lease consulted by `sweep_stale_sensors`.