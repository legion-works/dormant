---
kind: fix
surfaces: [cli]
issues: []
---
Detail: `dormantctl doctor` names the display a check refers to, so two
displays' results for the same probe can be told apart. The doctor tests no
longer share a process-global offline-invocation counter, so they pass under
plain `cargo test` as well as nextest.
