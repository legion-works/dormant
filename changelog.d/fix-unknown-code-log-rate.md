---
kind: fix
surfaces: [daemon]
issues: [409]
---
Detail: `coord_poll_unknown_code` is now logged at most once every 30 seconds for each display. Each line lists every unknown input code seen since the previous line (`codes`) and the number of unknown reads (`unknown_reads`). A panel without a signal scans its inputs, so the codes alternate. Before, a scan wrote one WARN line every 2 to 4 seconds while it continued.
