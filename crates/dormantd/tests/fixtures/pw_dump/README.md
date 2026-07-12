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
is the orphan-stream edge case. T4's hand-edited derivatives
(`role_missing.json`, `unknown_state.json`, `music.json`) are produced
FROM these by documented edits — not captured.
