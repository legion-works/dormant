kind: fix
surfaces: [render, daemon, webui]
---
Detail: Three intermittent tests no longer race on wall-clock or
cross-thread frame delivery. The dormant-render screensaver test
drains into the verification buffer instead of relying on a single
post-drain call to land on Ok(false) — mpv's internal render thread
can set MPV_RENDER_UPDATE_FRAME between two of our calls, and a
single follow-up call was not guaranteed to land on the no-new-frame
path. The web config-apply integration test drains the apply's
debounced reload before the backup-write verification window, so a
reload scheduled by the apply's own GET roundtrip no longer bleeds
into the assertion that writes to backups/ must not trigger a
reload. The webui deep-link retry test drives the SettingsForm
mock's 300ms mount delay under fake timers, removing the wall-clock
ordering assumption between the mock's delayed mount and the test's
poll window that flaked under CPU contention.
