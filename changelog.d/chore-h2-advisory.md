---
kind: fix
surfaces: []
issues: []
---
Detail: Updated the `h2` dependency to 0.4.16, clearing RUSTSEC-2026-0258 — unbounded queueing of empty HTTP/2 DATA frames that could grow memory without limit. It reached the build through `hyper` under both the web UI's `axum` server and `reqwest`.
