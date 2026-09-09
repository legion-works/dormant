---
kind: fix
surfaces: [daemon]
issues: []
---
Detail: Contested panels now resume evaluation only after `flap_settle` of stable reported input, preventing continued flapping from causing repeated render churn.
