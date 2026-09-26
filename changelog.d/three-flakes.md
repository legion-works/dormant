---
kind: fix
surfaces: [render, daemon, webui]
issues: []
---
Detail: Three tests that failed intermittently under CPU load no longer depend
on timing. The screensaver drain test re-drains until mpv reports no new frame
before checking that the current picture is still drawn; the config-apply
integration test drains the apply's pending reload before asserting that
writes to `backups/` trigger none; the web deep-link retry test advances fake
timers instead of waiting on the wall clock.
