---
kind: fix
surfaces: [deps]
issues: []
---
Detail: rustls moves from 0.23.45 to 0.23.46, which resolves RUSTSEC-2026-0346 (session secrets in `Debug` output), RUSTSEC-2026-0347 (application data sent to the outer server after an ECH rejection), RUSTSEC-2026-0348 (unbounded Brotli certificate decompression) and RUSTSEC-2026-0349 (ServerHello downgrade to classical key exchange). dormant reaches rustls only through HTTPS clients such as the Samsung TV's port 8002 WebSocket.
