---
kind: fix
surfaces: []
issues: [235]
---
Detail: migrate the web dashboard to axum 0.8 and unify the WebSocket stack on tungstenite 0.29, so one WebSocket implementation now serves the dashboard, the Home Assistant sensor, and the Samsung TV controller instead of three. Duplicate crate versions drop from 18 to 17, and `thiserror` collapses to a single major.
