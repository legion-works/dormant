---
kind: fix
surfaces: []
---

Web Apply no longer falsely rejects shared-display configs (the apply
route validated with an empty input-source-reader set, unlike the
daemon). Also a batch of operator-reported UI fixes: event badge
overflow, the history sentinel rendering as a raw row, unstyled action
buttons, the shared-blank warning displacing the actions row, and
display-detail card padding.
