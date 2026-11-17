---
kind: fix
surfaces: [daemon]
issues: [409]
---
Detail: Coordination poll warnings now name the cause of a failed read (a read error with its text, a sampler-priority skip, or a missing readback), and an input code that is neither the local nor the peer code is logged once when it appears or changes.
