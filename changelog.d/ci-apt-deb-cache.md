---
kind: improvement
surfaces: []
issues: [346]
---
Detail: Ubuntu CI reuses downloaded apt packages for the same runner image and package list. The existing bounded retries still apply when packages are missing from the cache.
