---
kind: fix
surfaces: [deps]
issues: []
---
Detail: Dependency updates resolve three advisories that make `cargo deny` fail on every pull request. rustls-webpki moves from 0.103.15 to 0.103.16 (RUSTSEC-2026-0382, certificate paths with distrusted emSign intermediates), webpki-roots from 1.0.9 to 1.0.11 (RUSTSEC-2026-0381, bundled distrusted emSign roots), and tokio from 1.53.1 to 1.54.1 (RUSTSEC-2026-0386, `AsyncFd::new` violates I/O safety). dormant reaches rustls only through HTTPS clients such as the Samsung TV's port 8002 WebSocket.
