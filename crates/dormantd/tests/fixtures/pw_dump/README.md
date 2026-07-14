# pw-dump classifier fixtures

Real `pw-dump` (PipeWire 1.6.7) captures from the maintainer's system
(icebox — AMD Ryzen, KWin 6.7.2 Wayland), taken 2026-07-12 during the
audio-aware-blanking T3 probe. Full findings + signal table:
`docs/research/2026-07-12-pipewire-probe.md`.

**Trim:** each capture was reduced to only `PipeWire:Interface:Node`
objects (`jq '[.[] | select(.type | endswith(":Node"))]'`). Every
classifier signal lives on Node objects — `media.class`
(`Stream/Output/Audio` = playback, `Stream/Input/Audio` = capture),
node `state` (`running` vs `idle`/`suspended`), `media.role`, and
`application.name`. Client/Port/Link/Factory/Module/Device/Metadata
objects carry no classifier signal and were dropped (~240 KB → ~85 KB
each). Raw captures are out-of-repo spike artifacts.

| Fixture | Scenario | Key signal |
|---|---|---|
| `idle.json` | Idle desktop, no media | 0 `Stream/*/Audio` nodes → `playback=false, call=false` |
| `movie.json` | mpv playing (sine) | 1 `Stream/Output/Audio` `state=running` → `playback=true`; `media.role` EMPTY (mpv sets none) |
| `movie_paused.json` | mpv paused via IPC | same node id, `state=idle` → NOT playback (state, not a corked field, is the discriminator) |
| `call.json` | 2× mpv out + 1× pw-record in | mixed `running`/`suspended` outputs + a `Stream/Input/Audio` running |
| `mic_only.json` | pw-record capture only | `Stream/Input/Audio` `state=running`, `media.role=Music` (NOT `Communication` → default `call_roles=["Communication"]` correctly excludes it) |
| `idle_dirty.json` | orphan SoX node (dead process) | `Stream/Output/Audio` `state=running` on a dead process — the false-positive edge the classifier must tolerate |

`movie.json`/`movie_paused.json`/`call.json`/`mic_only.json`/`idle.json`
are the five the T4 classifier `include_str!`s (plan P11). `idle_dirty.json`
is the orphan-stream edge case, also `include_str!`'d by T4 (fits the
matrix naturally — same role-missing/playback=true signal as `movie.json`,
proving the classifier doesn't attempt process-liveness checks).

## T4 hand-edited derivatives (P11 — never invented from scratch)

Each of the three fixtures below is a **single-node** JSON array trimmed
and/or field-edited FROM `movie.json`'s id=80 mpv `Stream/Output/Audio`
node (movie.json's own 6 non-stream driver/sink/midi nodes carry no
classifier signal and are dropped for these — a further trim than the
Node-only trim above). Edits are noted per file; nothing here is a fresh
invention — every byte traces to the real `movie.json` capture or, for the
borrowed role string, to another real capture (`mic_only.json`).

| Fixture | Derived from | Edit | Purpose |
|---|---|---|---|
| `role_missing.json` | `movie.json` id=80 node | None — pure trim (drop the 6 non-stream nodes; keep the mpv node byte-identical, `state=running`, no `media.role` key) | Isolated single-purpose fixture pinning "role-missing running output ⇒ playback=true" (spec §4.2) without the surrounding movie-scenario nodes |
| `unknown_state.json` | `movie.json` id=80 node | `info.state` changed from `"running"` to `"draining"` — an invented, out-of-spec string; real PipeWire only ever emits `running`/`idle`/`suspended` for stream nodes (probe doc), so this exact value never occurs in nature | Pins spec F5: a stream-class node with an unrecognized/missing `state` is treated as RUNNING, never escalated to a whole-poll error |
| `music.json` | `movie.json` id=80 node | `info.props["media.role"]` ADDED as `"Music"` — mpv itself sets no role (probe finding), but `"Music"` is a REAL role string observed on `mic_only.json`'s pw-record node, borrowed here to simulate a hypothetical music-player app (no real one was captured) | Pins `playback_roles` narrowing (spec §5.1 F16): `playback_roles = Some(["Movie"])` must NOT inhibit a running output whose role is `"Music"` |

**Known plan/fixture drift (documented, not silently resolved):** the plan's
Task 4 Step 1 text states `call.json` + default `call_roles` should yield
`call=true`. The real capture contradicts this: `call.json`'s only
running input node (pw-record) has `media.role="Music"`, not
`"Communication"`, and under the default config (`call_roles =
["Communication"]`, `capture_is_call = false`) it therefore classifies as
`playback=true, call=false` — identical in kind to `movie.json`, per this
fixture's own probe doc entry (`docs/research/2026-07-12-pipewire-probe.md`
state → signal table: `call-standin | true | false | true`). No real
capture exercises the role-based call path (`media.role` ∈ `call_roles`);
the probe doc's own "Open items" section flags real-call verification as
still outstanding. T4's tests assert the REAL fixture content, not the
plan's stale expectation.
