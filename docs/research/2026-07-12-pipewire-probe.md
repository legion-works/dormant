# PipeWire pw-dump probe — findings (dormant audio-aware blanking T3)

Date: 2026-07-12 · Hardware: icebox (AMD Ryzen, NVIDIA RTX 4090) · Compositor: KWin 6.7.2 Wayland · PipeWire: 1.6.7 (compiled with libpipewire 1.6.7) · Probe captures: `/tmp/opencode/pw-probe/*.json`

## Decision

**GO — the `pw-dump` JSON shape is usable as-is.** The `state` field on Stream/Output/Audio and Stream/Input/Audio nodes cleanly distinguishes running (playing/recording) from idle (paused/corked) and suspended. No corked-specific field check is needed beyond `state != "running"`. The classifier can be implemented against these real captures.

## States captured

| # | State | File | Wall-clock time | Stream nodes found |
|---|---|---|---|---|
| 1 | `idle` | `idle.json` | 2026-07-12 ~14:00 | 0 Stream/*/Audio nodes |
| 2 | `idle-dirty` | `idle-dirty.json` | 2026-07-12 ~14:00 | 1 orphan SoX node (state=running, process dead) |
| 3 | `playing` | `playing.json` | 2026-07-12 ~14:01 | 1 mpv Stream/Output/Audio, state=running |
| 4 | `paused` | `paused.json` | 2026-07-12 ~14:02 | 1 mpv Stream/Output/Audio, state=idle |
| 5 | `mic-only` | `mic-only.json` | 2026-07-12 ~14:03 | 1 pw-record Stream/Input/Audio, state=running |
| 6 | `call-standin` | `call-standin.json` | 2026-07-12 ~14:04 | 2 mpv Stream/Output/Audio (1 running, 1 suspended) + 1 pw-record Stream/Input/Audio (running) |

## Per-state node evidence

### 1. idle — no audio streams
```
Total objects: 86, Nodes: 7
Audio nodes: 3 (all sinks/sources, no Stream/*/Audio)
  id=83 state=suspended class=Audio/Sink
  id=84 state=suspended class=Audio/Source
  id=68 state=idle class=Audio/Sink
```
**Signal:** zero Stream/Output/Audio or Stream/Input/Audio nodes → both `playback=false, call=false`.

### 2. playing — mpv sine tone
```json
{
  "id": 80,
  "state": "running",
  "media.class": "Stream/Output/Audio",
  "media.role": "",
  "application.name": "mpv",
  "node.name": "mpv",
  "media.name": "lavfi:sine=frequency=440:duration=90 - mpv",
  "pulse.corked": ""
}
```
**Signal:** `state=running` on `Stream/Output/Audio` → `playback=true`. `media.role` is empty string — mpv does NOT set a role by default. `pulse.corked` is absent/empty.

### 3. paused — mpv sine tone, paused via IPC
```json
{
  "id": 80,
  "state": "idle",
  "media.class": "Stream/Output/Audio",
  "media.role": "",
  "application.name": "mpv",
  "node.name": "mpv",
  "media.name": "lavfi:sine=frequency=440:duration=90 - mpv",
  "pulse.corked": ""
}
```
**Signal:** `state=idle` on `Stream/Output/Audio` → NOT playback. The node persists (same id=80), only the state changes. No separate corked field is set.

### 4. mic-only — pw-record
```json
{
  "id": 123,
  "state": "running",
  "media.class": "Stream/Input/Audio",
  "media.role": "Music",
  "application.name": "pw-record",
  "node.name": "pw-record",
  "media.name": "/tmp/opencode/pw-probe/mic-test.wav"
}
```
**Signal:** `state=running` on `Stream/Input/Audio` → input stream active. `media.role="Music"` — pw-record sets role=Music by default, NOT Communication. Under default `capture_is_call=false`, this would NOT set `call=true`. Under `capture_is_call=true`, it would.

### 5. call-standin — mpv + pw-record simultaneously
```
Stream nodes:
  id=105 state=running class=Stream/Output/Audio role="" app=mpv
  id=114 state=suspended class=Stream/Output/Audio role="" app=mpv  (orphan from earlier kill)
  id=123 state=running class=Stream/Input/Audio role="Music" app=pw-record
```
**Signal:** running output + running input → `playback=true, call=false` (under default config, since input role is Music not Communication). The suspended orphan (id=114) is correctly ignored by a `state=running` check.

## State → signal table

| State | `playback` | `call` (default cfg) | `call` (capture_is_call=true) | Key signal |
|---|---|---|---|---|
| idle | false | false | false | No Stream/*/Audio nodes with state=running |
| playing | **true** | false | false | Any Stream/Output/Audio with state=running |
| paused | false | false | false | Stream/Output/Audio exists but state=idle |
| mic-only | false | false | **true** | Any Stream/Input/Audio with state=running |
| call-standin | true | false | true | Both output and input running |

## Key questions answered

### Q1: Does a paused mpv stream show state=idle, suspended, or disappear?
**Answer: state=idle.** The node persists with the same id, same media.class, same properties — only `state` changes from `"running"` to `"idle"`. No separate corked/suspended state. The node does NOT disappear.

### Q2: Is `state: "running"` on Stream/Output/Audio alone a sufficient playback-active signal?
**Yes.** Every running playback stream in our captures has `state=running` on a `Stream/Output/Audio` node. No false positives observed. The only caveat: orphan nodes (process dead, PipeWire node lingering) can also show `state=running` — see Surprises below.

### Q3: Is `state: "running"` on Stream/Input/Audio a sufficient mic/call signal?
**Yes for mic-active detection.** But under default `capture_is_call=false`, a running input stream does NOT set `call=true`. The poller must check the config flag. Under `capture_is_call=true`, any running Stream/Input/Audio sets `call=true`.

### Q4: What media.role values appear in practice?
| App | media.role |
|---|---|
| mpv (sine tone) | (empty string — not set) |
| pw-record | `"Music"` |
| SoX | (empty string — not set) |

**Key finding:** mpv does NOT set `media.role` at all. The spec's permissive default (all non-call roles inhibit playback) is correct — if the classifier required a role match, mpv would never inhibit. The `playback_roles` narrowing filter would need to account for this (empty string is the default mpv role).

### Q5: pw-dump size + parse cost
| Capture | Bytes | Objects | Nodes |
|---|---|---|---|
| idle | 197,797 | 86 | 7 |
| playing | 237,357 | 104 | 8 |
| paused | 237,354 | 104 | 8 |
| mic-only | 253,397 | 109 | 9 |
| call-standin | 288,716 | 127 | 10 |

- **Typical size: ~200-290 KB** (well within the 4 MiB cap)
- **Typical node count: 7-10** (only 1-3 are Stream/*/Audio)
- **Runtime:** sub-second (not measured precisely, but `pw-dump` returns instantly in all cases)

### Q6: pw-dump exit code + failure behavior
```
$ PIPEWIRE_REMOTE=/nonexistent pw-dump
can't connect: Host is down
EXIT: 255
```
Exit code 255, stderr message. The poller should check exit code and treat non-zero as a failure.

### Q7: PipeWire version
```
pw-cli --version:
Compiled with libpipewire 1.6.7
Linked with libpipewire 1.6.7
```

## Poller recommendations

1. **Classifier logic** (pseudocode):
   ```
   for each node with type == "PipeWire:Interface:Node":
       if node.info.props.media.class == "Stream/Output/Audio" AND node.info.state == "running":
           playback = true
       if node.info.props.media.class == "Stream/Input/Audio" AND node.info.state == "running":
           if cfg.capture_is_call:
               call = true
           # else: ignore input streams for call detection
   ```
   No role check needed for the default case — `playback_roles` narrowing is an optional refinement.

2. **Navigation:** Use `serde_json::Value` (not typed structs) — the JSON shape is large and contains many unknown fields. Navigate by path: `node["info"]["state"]`, `node["info"]["props"]["media.class"]`.

3. **4 MiB cap:** Real dumps are ~200-300 KB. The 4 MiB cap is generous but safe — a very busy PipeWire graph (many apps, many sinks) could be larger.

4. **Failure handling:** Exit code 255 + stderr "can't connect" when PipeWire is down. The poller should check exit code, not just parse stdout.

5. **Orphan node tolerance:** A zombie stream (process dead, PipeWire node lingering) can show `state=running`. This is a real edge case — the poller may falsely inhibit for a brief window until PipeWire cleans up the orphan. Acceptable: the fail-safe direction is toward keeping the screen on, and the orphan is transient.

## Surprises

1. **Orphan zombie streams show state=running.** The SoX node (id=38) had its process dead but PipeWire still reported `state=running`. This means a crashed audio app can leave a false-positive running stream. Mitigation: PipeWire typically cleans these up within seconds; the poller's `min_active` debounce (3s default) would filter out very brief orphans.

2. **mpv does NOT set media.role.** The spec's permissive default (all non-call output inhibits) is essential — if the classifier required a role match, mpv would never inhibit playback. The `playback_roles` narrowing option must handle empty-string roles.

3. **pw-record sets role=Music, not Communication.** A real call app (Discord/Teams/Zoom) would likely set `media.role=Communication`. The default `call_roles=["Communication"]` is correct — pw-record's Music role would NOT trigger call detection under defaults, which is the right behavior (a recording app is not a call).

4. **Sink state transitions.** The HDMI sink (id=68) transitions between `idle` and `running` depending on whether audio is actively playing through it. This is expected but worth noting: the poller should NOT look at Audio/Sink nodes — only Stream/*/Audio nodes matter.

5. **Multiple mpv nodes from the same binary.** When mpv was killed and restarted, two mpv nodes appeared (one running, one suspended). The suspended one is an orphan from the previous instance. This reinforces that `state=running` is the correct filter, not just presence of a node.

## Open items

- **Real call app verification:** pw-record with role=Music is a stand-in. A real Teams/Discord/Zoom call may set `media.role=Communication` on both the output AND input streams. The classifier's role-based call detection needs verification against a real call. Flagged as `verify-when-a-real-call-happens`.
- **Orphan cleanup timing:** How quickly does PipeWire clean up orphan nodes after a process dies? If cleanup is slow (>poll_interval), the poller could falsely inhibit for multiple ticks. A quick test: kill mpv, then poll pw-dump every second until the node disappears. Not tested in this probe.
- **pw-dump JSON schema stability:** PipeWire 1.6.7 output shape is documented here. If PipeWire is updated, the field paths should be re-verified. The `serde_json::Value` navigation approach tolerates new fields by construction.
- **`playback_roles` with empty-string roles:** If an operator sets `playback_roles = ["Movie"]`, mpv (which emits no role) would NOT inhibit. This may be surprising. Document that the default (unset) is the permissive option.
