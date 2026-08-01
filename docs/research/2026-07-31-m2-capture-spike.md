# M2 capture spike — KDE Wayland ScreenShot2 vs ScreenCast

Date: 2026-07-31  
Host/session: KDE Plasma Wayland, `wayland-0`, AOC output DP-1  
Scope: read-only live probes; all throwaway code and images were under `/tmp/m2-capture-spike/`.

## Decision

**GO — xdg-desktop-portal ScreenCast with a persisted restore token is the capture path.**
It requested one monitor-consent interaction, returned a PipeWire remote and restore token,
and a second session reattached with the token and produced a real frame. The native KWin
ScreenShot2 path is also technically viable, but its executable-to-desktop-file authorization
is packaging-sensitive and is not a good sole path for a plain daemon.

The source-side stream cannot be negotiated down to 320×180 for an output stream on this KWin:
the compositor advertises a fixed native size for `OutputScreenCastSource`. The production
implementation should therefore keep the portal stream open at native size, sample at a low
cadence, and reduce the frame immediately. The Python reduction benchmark is expensive enough
to justify a native Rust/SIMD implementation and/or a compositor-side future feature, but it is
not a blocker for a periodic sample.

## Q1 — KWin ScreenShot2 authorization

### Evidence

Live interface introspection:

```
$ busctl --user introspect org.kde.KWin /org/kde/KWin/ScreenShot2 org.kde.KWin.ScreenShot2
.CaptureActiveScreen method   a{sv}h     a{sv}        -
.CaptureScreen       method   sa{sv}h     a{sv}        -
.Version             property u          5            emits-change
```

An unauthorized Python `dbus` call supplied a valid output FD and empty options:

```
EXCEPTION_TYPE DBusException
EXCEPTION org.kde.KWin.ScreenShot2.Error.NoAuthorized: The process is not authorized to take a screenshot
OUTPUT_SIZE=0
```

Installed desktop entries declaring the restricted interface:

```
/usr/share/applications/org.freedesktop.impl.portal.desktop.kde.desktop:X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
/usr/share/applications/org.kde.plasmashell.desktop:X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
/usr/share/applications/org.kde.spectacle.desktop:X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
```

Spectacle's entry is an ordinary installed application entry:

```
Name=Spectacle
Exec=/usr/bin/spectacle
DBusActivatable=true
X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2
```

Upstream KWin source (`src/plugins/screenshot/screenshotdbusinterface2.cpp`, fetched from
the Plasma/KWin `master` tree) shows the caller identity lookup:

```cpp
const QDBusReply<uint> reply = connection().interface()->servicePid(message().service());
```

The exact authorization matcher was not present in that plugin source. Independent upstream
prior art (Unisic's `src/capture/KWinScreenShot2.cpp` and `AGENTS.md`) documents the matcher as
the caller's `/proc/<pid>/exe` path against an installed desktop entry's `Exec`. A search result
for the same KWin behavior states that it compares the desktop-file `Exec` with the executable
location obtained from procfs. Treat this executable-match detail as corroborated, not as a
primary KWin-source citation.

### Verdict — **viable-but-awkward**

`dormantd` would need a desktop entry containing
`X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2`, with `Exec` resolving to the
exact daemon executable path that makes the D-Bus call. A `systemd --user` unit name,
`SystemdService=`, or an app-id alone is not evidence that the process satisfies this check.
This is achievable in a packaged install, but adds desktop-entry/cache and path-match failure
modes. Keep it as an opportunistic native KDE fast path, never as the only capture path.

## Q2 — ScreenCast portal, persist mode 2, restore token

Probe: `/tmp/m2-capture-spike/portal_capture_probe.py`. It used the system Python `dbus` and
GI/GStreamer bindings. The initial `SelectSources` requested `types=monitor`, `multiple=false`,
`cursor_mode=hidden`, and `persist_mode=2`. The operator approved the one initial monitor
selection. `OpenPipeWireRemote` supplied the FD; `pipewiresrc path=<node_id>` consumed the
portal-private node (the `path` property is required here; `target-object` returned “target not
found”).

Initial session output:

```
INITIAL_STREAMS_COUNT=1
INITIAL_STREAM_0_NODE_ID=141
INITIAL_STREAM_0_SOURCE_TYPE=1
INITIAL_STREAM_0_SIZE=dbus.Struct((dbus.Int32(3072), dbus.Int32(1728)), ...)
INITIAL_RESTORE_TOKEN_PRESENT=True
INITIAL_RESTORE_TOKEN_LENGTH=22
INITIAL_PIPEWIRE_FD_TYPE=UnixFd
FRAME_ELAPSED_MS=213.770
FRAME_PNG_BYTES=2522773
INITIAL_CLOSED=1
```

The PNG IHDR was `3840 × 2160`, 8-bit RGBA, non-interlaced. The 3072×1728 stream property is
the compositor-coordinate size; the output pixel size is 3840×2160 at this session's scale.

Restore session output:

```
RESTORE_STREAMS_COUNT=1
RESTORE_STREAM_0_NODE_ID=141
RESTORE_STREAM_0_SOURCE_TYPE=1
RESTORE_STREAM_0_SIZE=dbus.Struct((dbus.Int32(3072), dbus.Int32(1728)), ...)
RESTORE_RESTORE_TOKEN_PRESENT=True
RESTORE_RESTORE_TOKEN_LENGTH=22
RESTORE_PIPEWIRE_FD_TYPE=UnixFd
RESTORE_ATTACHED=1
FRAME_ELAPSED_MS=241.032
FRAME_PNG_BYTES=2476719
RESTORE_CLOSED=1
```

The second `SelectSources`/`Start` request returned a node and FD without a second consent
interaction in this run. The probe did not instrument the portal UI itself, so the residual
risk is limited to proving “no dialog” from the request exchange rather than visually observing
the absence; the runtime implementation should log request completion and treat a non-zero
response as a capture failure.

### Verdict — **GO: token reattach works headlessly in this session**

Runtime story: first run asks for monitor consent and stores the returned restore token in
daemon state. Close the session after each sample or keep it open for a sampling window. On
restart/reconnect, create a session and pass the token to `SelectSources` with `persist_mode=2`;
do not ask the user again unless the token is rejected or the monitor topology changed.

## Q3 — frame and luma cost; source-size negotiation

### Measured costs

The portal/GStreamer one-buffer timings include pipeline setup, PipeWire connection, format
negotiation, 4K transfer, conversion, PNG encoding, and teardown. They are therefore an upper
bound for a warm long-lived consumer, not a per-frame steady-state cost:

| operation | result |
|---|---:|
| acquire one 4K frame through portal + GStreamer | 213.770 ms initial; 241.032 ms restore run |
| 4K frame dimensions | 3840×2160 RGBA |
| NumPy sRGB→linear + Rec.709 luma + exact 240×240 block reduction | median 173.431 ms; p95 180.329 ms |
| output grid | 16×9 |

Benchmark output:

```
INPUT_SHAPE (2160, 3840, 3)
GRID_SHAPE (9, 16)
LUMA_MS_MEDIAN 173.431
LUMA_MS_P95 180.329
LUMA_MS_MIN_MAX 169.653 184.866
```

The benchmark was intentionally straightforward NumPy float32 math; Rust can avoid temporary
arrays and use row/block accumulation. A native implementation should be benchmarked before
setting the sample cadence.

The production per-tick cost is expected to be warm-stream frame acquisition plus a Rust
sRGB-linear reduction of one unavoidable 3840×2160 RGBA buffer to 16×9; 173 ms is a NumPy
artifact, not the Rust target. The first M2 implementation decision is whether to hold the
PipeWire stream warm between 30-second ticks (paying the unmeasured idle-CPU cost above) or tear
it down and re-acquire each tick (paying stream setup latency); this spike does not resolve it.

### Can PipeWire request 320×180 from KWin?

**No for this output source, based on KWin's source-level negotiation contract.** In upstream
`src/plugins/screencast/screencaststream.cpp`, `buildFormats()` sets `minSize` and `maxSize` to
`defaultSize` whenever `m_source->followsStreamSize()` is false. `ScreenCastSource::followsStreamSize()`
returns false by default, and `OutputScreenCastSource` does not override it. Thus an output stream
advertises its native pixel size, not a 200×200…10000×10000 range. `onStreamParamChanged()` does
accept a negotiated size and renders into the target, but the output source does not advertise
a smaller choice. The live probe consequently received native 3840×2160; no 320×180 source
buffer was observed.

### Idle CPU

Closed-state 60-second sample (no portal stream):

```
kwin_wayland_CPU_PERCENT=1.383
pipewire_CPU_PERCENT=0.083
wireplumber_CPU_PERCENT=0.000
ELAPSED_SECONDS=60.000
```

An open-but-idle 60-second sample was **not run** after the successful token probe: obtaining a
new initial session risked a second consent prompt, and the successful probe closed its session
immediately after each frame. Do not treat the closed baseline as an open-stream measurement.
KWin's source connects `OutputScreenCastSource::frame` to output repaint scheduling, so an idle
desktop should avoid repeated scene renders, but the actual open-stream CPU delta remains a
follow-up measurement in the real daemon prototype.

### Verdict — **GO with native reduction; NO-GO for compositor-side 320×180 assumption**

Request native frames and reduce in Rust. Do not design around PipeWire negotiating a small
source buffer until KWin changes its output-source size advertisement.

## Q4 — idle stream cost

**Measured.** The daemon-identity gate below covers both closed and warm-paused idle cost over
30-minute windows.

### 2026-08-01 M2 active-sampling daemon-identity premise gate — **PASS**

`dormant.service` was verified as the active graphical-user service before the probe: its unit
file was `/home/icetea/.config/systemd/user/dormant.service`, its `ExecStart` was
`/home/icetea/.local/bin/dormantd`, and the unit was active under the operator's user manager.
The temporary probe was built into that exact executable, installed at that exact path, and run
only through `systemctl --user restart dormant.service`.

The first corrected start minted a restore token after one consent dialog. Restarting the same unit
then logged `token_reattached`, `restore_token_saved`, and `pipewire_remote_opened` without another
dialog. `identity_reattach = pass` under the real daemon identity.

The earlier `NoReply` result was a probe bug, not an identity failure. Its malformed
`CreateSession` request omitted `session_handle_token`, triggering the xdg-desktop-portal
`xdp-session.c:296` assertion and three portal coredumps. Supplying unique request and session
tokens fixed the request. A second probe bug dropped the D-Bus connection after opening the
PipeWire remote, which destroyed the portal session and produced `no target node available` when
the stream activated. Retaining that connection for the stream lifetime fixed activation.

Both CPU windows used the same KDE session and stable process IDs (`kwin_wayland` 10602,
`pipewire` 1170) with `USER_HZ=100`:

| State | Duration | `kwin_wayland` ticks / CPU | `pipewire` ticks / CPU |
| --- | ---: | ---: | ---: |
| Closed | 1806.29 s | 12377→15259 / 1.596% | 1831→2009 / 0.099% |
| Warm, stream paused | 1801.71 s | 17410→20125 / 1.507% | 2117→2281 / 0.091% |
| Warm − closed | — | −0.089 percentage points | −0.008 percentage points |

The closed window entered grace several times but never reached `staged` or `blanked`. The warm
window also had no `staged` or `blanked` transition; `dormantctl status` reported daemon blanking
paused after the restart. The display and compositor therefore stayed active in both windows,
which preserves the comparison despite the different daemon pause flag.

Thirty pause→resume→first-frame samples produced p95 **14.377 ms** (minimum 5.627 ms, maximum
172.557 ms; nearest-rank p95). Neither process exceeded the `> 0.5%` warm-idle delta threshold, so
the default is **`warm`**. Both stream modes remain available.

## Q5 — composited output and brightness correlation

**GO for content-luma, not panel-luma.** KWin's output screencast source renders a
`FilteredSceneView` into a target framebuffer (`outputscreencastsource.cpp`: `sceneView->paint`),
then PipeWire exports that rendered image. This is the composited desktop scene — windows,
panels, decorations, and compositor effects — rather than a pre-compositing application buffer.
The capture is also before physical panel processing: display LUT/night-color/gamma behavior
applied at scanout is not represented as measured panel light. Document the 16×9 values as
**content-luma/compositor-luma**, not calibrated OLED brightness.

The captured PNG files were real 3840×2160 RGBA frames from the live desktop. No windows, display
power, DDC, or color settings were manipulated.

## Path comparison and recommendation

| path | silent after setup | periodic frame | small source buffer | recommendation |
|---|---|---|---|---|
| KWin ScreenShot2 | yes, if executable↔desktop entry authorization matches | yes, one D-Bus raw frame per sample | not applicable | opportunistic KDE fast path |
| Portal ScreenCast + restore token | one initial consent; token reattach worked | yes, PipeWire stream | no on this KWin output source | **primary implementation** |

The daemon should use ScreenCast as the portable, recoverable path: initial consent is explicit,
restore-token reattach avoids repeated prompts, and the stream can be sampled periodically while
the session remains open. A KWin ScreenShot2 attempt can be used when a matching installed
desktop entry is guaranteed; on authorization failure, fall back immediately to the portal rather
than retrying the restricted D-Bus call.

### Explicit recommendation gates

* **GO — ScreenCast + restore token:** primary path; one consent, then headless token reattach.
* **Viable-but-awkward — KWin ScreenShot2:** secondary fast path; packaging burden and the
  executable/`Exec` match are corroborated by prior art, not primary KWin-source evidence.
* **NO-GO — Screenshot portal:** not used; ScreenCast supplies the persistent stream needed here.
* **NO-GO — source-size negotiation:** this output source does not advertise 320×180.
* **GO — attribution:** document the grid as composited content-luma before panel LUT/night-color
  processing; that is sufficient for wear distribution, not calibrated brightness.

## Probe cleanup

All probe scripts and frames remain quarantined in `/tmp/m2-capture-spike/` for inspection. The
portal sessions were closed. Final process check:

```
$ pgrep -a -f 'portal_capture_probe|gst-launch|pw-record|pw-cat|pipewiresrc' || true
(no matching probe process)
```
