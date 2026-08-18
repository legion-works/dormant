---
kind: improvement
surfaces: []
issues: []
---
Detail: CI installs its native build dependencies through a wrapper that bounds each `apt-get` attempt and retries, so a stalled Ubuntu archive mirror costs one short attempt instead of a job's entire time budget.
