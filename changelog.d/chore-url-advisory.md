---
kind: fix
surfaces: []
issues: []
---
Detail: Updated the `url` dependency to 2.5.9, clearing RUSTSEC-2026-0592: `Url::origin()` recursed once for each nested `blob:` prefix, so a crafted URL could overflow the stack. It reaches the build through `reqwest`.
