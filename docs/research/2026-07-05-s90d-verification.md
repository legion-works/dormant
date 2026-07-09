# Samsung S90D — Hardware Spike Verification

> Spike conducted 2026-07-05 on host `192.0.2.7` (model QA65S90DAKXXA "Frozen Mirror").
> Firmware/platform: Tizen OS. Ports tested: 8002 (WSS remote), 8001 (REST device-info).

## Summary

`KEY_PICTURE_OFF` blanks the panel while audio continues. This is the core
capability dormant needs for a TV that doubles as an audio sink. The TV has
two standby depths with different socket behaviors, so socket reachability
alone does not tell you whether the panel is actually on.

**Verdict:** GO for the `KEY_PICTURE_OFF` strategy. The controller must hold a
persistent WebSocket (with keepalive), reconnect on send failure, and derive
real panel state from the REST `/api/v2/` PowerState field, not from socket
liveness.

## Connection behavior

Paired via `samsungtvws`; token persisted successfully. The TLS certificate is
self-signed — the controller must accept it.

| Metric | Value |
|---|---|
| Cold connect + send latency | 1.16–3.91 s across multiple sends |
| Reconnect budget (target) | ≤1.5 s |
| Implication | Cold latency mostly exceeds a reconnect budget → hold a persistent socket; do not reconnect per command |

## Socket survival under idle

Held the WebSocket open for 10 minutes during `KEY_PICTURE_OFF` state. The TV
silently dropped the idle socket (`BrokenPipe` on the next send, with no error
or close frame until that send). A wake on the held socket failed; a fresh
connection succeeded.

**Design mandate:** the controller must send periodic keepalive pings and
reconnect on any send failure. Never trust an idle socket.

## Two standby depths

The TV has two off-states with different socket behavior:

| State | Socket (port 8002/8001) | PowerState (REST) | Wake method |
|---|---|---|---|
| Awake, picture-off | Open (drops when idle, see above) | `"on"` | `KEY_RETURN` (or `KEY_PICTURE_OFF` toggle) |
| Warm network-standby | Open (~0.30 s connect) | `"standby"` | `KEY_POWER` over WS |
| Deep standby | Connection refused (~1 s) | Unreachable | WoL magic packet |

- `KEY_RETURN` is the preferred wake key from picture-off: it is not a toggle,
  so sending it to an already-awake panel is harmless (idempotent wake).
- `KEY_PICTURE_OFF` toggles picture on/off — safe as a wake from picture-off,
  but would blank an awake panel if daemon state drifts.
- WoL magic packet woke the TV from deep standby (verified on this TV).
  The TV has two MACs: ethernet `<ethernet-mac>`, Wi-Fi `<wifi-mac>`.
  Send WoL to the ethernet MAC for best reliability.

## Panel state discrimination

The authoritative panel-state source is the REST endpoint:

```
GET http://192.0.2.7:8001/api/v2/
```

Parse `device.PowerState`: `"on"` means the panel is on (picture may be off);
`"standby"` means the TV is in network-standby. When the REST endpoint responds,
always trust its PowerState over socket liveness.

For dormant's doctor check:

| Connection result | Interpretation |
|---|---|
| Refused (~1 s) | Standby or off |
| Timeout | Unreachable or wrong IP |
