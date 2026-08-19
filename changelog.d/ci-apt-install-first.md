---
kind: improvement
surfaces: []
issues: []
---
Detail: CI now installs from runner-provided apt lists before refreshing them, so a stalled repository-index fetch is skipped when packages are already resolvable; stale lists still refresh and retry, and timeout messages identify the blocked phase.
