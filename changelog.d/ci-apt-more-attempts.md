---
kind: improvement
surfaces: []
issues: []
---
Detail: Package installation in CI now makes four independent attempts with shorter per-attempt budgets instead of three longer ones, since the observed failures are per-connection variance rather than a sustained outage. Every job still exhausts its attempts inside its own job timeout.
