---
kind: improvement
surfaces: [docs]
issues: [265]
---
Detail: documented the Windows platform story — what works (DDC/CI blanking, sensors, Web UI, GetLastInputInfo idle detection), what is absent (tray, render ladder, service supervision, watchdog, Samsung token persistence), the `%APPDATA%`/`%LOCALAPPDATA%` paths and named-pipe IPC, Task Scheduler rather than a service, and that the `command` controller runs through `cmd /C`.
