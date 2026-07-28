# MQTT state publishing

> **Status:** opt-in (issue #105). Off by default. The daemon never
> opens a broker connection on the publish plane unless
> `[publish] enabled = true`.

The publisher task forwards every retained state change and Home
Assistant MQTT discovery record to a broker of your choice. The
contract, sanitizer, and discovery payload shapes are pinned in
`.opencode/decisions/2026-07-27-mqtt-publish-contract.md`; the daemon
implements the same contract in `crates/dormantd/src/state_publisher.rs`.

## At a glance

- Single task per generation; reborn on every accepted reload — at
  most one publisher `client_id` connected at a time.
- Republishes the full discovery + current snapshot on every
  connect/reconnect. Broker restart loses nothing.
- `QoS 1`, `retain: true` for every state, availability, and discovery
  payload.
- Home Assistant MQTT discovery is fully wired — entities appear in
  HA's MQTT integration as soon as the broker reports the discovery
  topics.
- Graceful teardown publishes a retained `offline` on the global
  availability topic; the LWT handles ungraceful loss with the same
  payload.

## Configuration

The `[publish]` section in your `config.toml`:

```toml
[publish]
enabled        = true
broker_url     = "tcp://homeassistant.local:1883"
base_topic     = "dormant"        # default
discovery_prefix = "homeassistant" # default
instance_id    = "office-pc"      # default: $HOSTNAME
```

`enabled` is the kill-switch. When `false`, the daemon does not spawn
a publisher task, makes no MQTT connections on the publish plane, and
emits no discovery records.

`broker_url` accepts any form `dormant_core::mqtt::parse_broker_url`
parses — `tcp://host:1883`, `mqtt://host:1883`, or `host:1883`. The URL
is also the **exact** credential-lookup key (matching the MQTT sensor
convention in `crates/dormant-sensors/src/mqtt.rs`).

`base_topic` is the prefix every state topic is published under. The
default `dormant` puts sensor state at
`dormant/<instance>/sensor/<id>/state`. MQTT wildcards (`+` / `#`)
are rejected at validation time so a typo can never silently match
more topics than intended.

`discovery_prefix` is the HA discovery prefix; the default
`homeassistant` matches Home Assistant's MQTT integration settings.
Change it only when your HA instance is configured to use a different
discovery prefix.

`instance_id` is sanitized via the
`[sanitize_topic_id][crate::state_publisher::sanitize_topic_id]`
helper — every character outside `[a-z0-9_-]` becomes `_`, runs are
collapsed, and empty results fall back to `unnamed` (a deliberate
collision-warning signal). The raw value is preserved in the config
so operators can see their actual id in the web inventory and logs.
When unset, `instance_id` defaults to `$HOSTNAME` with a deterministic
`dormant` fallback.

## Credentials

Credentials live in the separate `credentials.toml` (the same file
the sensors use). Match the publish `broker_url` exactly:

```toml
[ha_token]
# dormant: optional HA long-lived token (not needed by the publisher)

[mqtt."tcp://homeassistant.local:1883"]
username = "dormant"
password = "..."
```

A missing `mqtt.<url>` entry allows an anonymous connect — same as the
sensors. dormant never publishes secrets; the password never appears
in any log line, traced span, or stringified form of the transport
surface.

## Topics

With the defaults, `instance_id = "office-pc"`, sensor id `desk`,
zone id `office`, display id `main`:

| Topic                                                          | QoS | Retain | Payload                                            |
| -------------------------------------------------------------- | --- | ------ | -------------------------------------------------- |
| `dormant/office-pc/availability`                              | 1   | yes    | `online` / `offline` (LWT + graceful cancel)       |
| `dormant/office-pc/sensor/desk/state`                         | 1   | yes    | `ON` / `OFF`                                       |
| `dormant/office-pc/sensor/desk/availability`                  | 1   | yes    | `online` / `offline` (per-sensor broker presence)  |
| `dormant/office-pc/zone/office/state`                         | 1   | yes    | `ON` / `OFF` (zone resolved presence)              |
| `dormant/office-pc/display/main/phase`                        | 1   | yes    | JSON `{"phase":"active"}` (see below)              |
| `homeassistant/binary_sensor/office-pc/sensor_desk/config`   | 1   | yes    | HA discovery (`binary_sensor`, see below)          |
| `homeassistant/binary_sensor/office-pc/zone_office/config`    | 1   | yes    | HA discovery                                        |
| `homeassistant/sensor/office-pc/display_main/config`         | 1   | yes    | HA discovery (`sensor`)                            |

The `display/.../phase` payload is a JSON object so HA's
`value_template: "{{ value_json.phase }}"` parses the literal phase
straight out of the published topic. Available literals:
`active | grace | blanking | blanked | waking | staged`.

Each HA discovery config declares `availability` with the global
LWT topic (`dormant/<instance>/availability`). Sensor entities
**additionally** reference their per-sensor availability topic with
`availability_mode: "all"` so HA marks a sensor unavailable the moment
its source goes offline, even while the global topic is still `online`.

## Collisions

If two distinct raw ids sanitize to the same topic id, the publisher
emits a single `publish_id_collision` WARN per colliding pair and
publishes **only the first** in config order — never interleaves two
entities on one topic. The collision report also surfaces in
`dormantctl validate` output so an operator can rename the offending
id before deploy.

Empty / whitespace ids both sanitize to `unnamed` and the collision
rule fires for them — a deliberate signal, not a free pass to the
`dormant` default.

## Reload behavior

A `[publish]` section edit (or any reload the validator accepts)
**tears the publisher down and re-spawns it with the new config**.
The old task is cancelled and awaited (`producer_token.cancel()`,
bounded 5s grace) so at most one client id is connected at a time
and the LWT fires `offline` for the old instance before the new
instance starts. This means a config typo in `[publish]` does not
brick the daemon — the previous generation stays online until the
new generation fails its daemon-side validation.

## What is NOT published

- **No secrets.** The password never appears in any log, span, or
  stringified form of any public type.
- **No operational events.** Blank failures, wake retries, wear
  samples, ownership transitions, and compensation advisories all
  have their own diagnostic surfaces (logs, IPC, web UI). The
  publisher carries state, never operational noise.
- **No `DaemonEvent::Unknown`.** The wire-tolerance catch-all variant
  publishes nothing — the publisher never speculates about events
  the engine did not emit.

## Doctor probe — out of scope

The `dormant-doctor` MQTT probe subscribes to a single sensor's
topic and reports broker presence as part of the coalesced
`dormant doctor` output. That probe is the **read-side** diagnostic
and is unaffected by the publisher; this page is the **write-side**.
Operators can verify the publisher is healthy by running
`dormant doctor` and confirming the broker probe still reports the
expected presence on the configured `broker_url`.
