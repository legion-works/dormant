---
kind: fix
surfaces: [cli, web]
issue: 195
---
`dormantctl pair samsung` and the web pairing route no longer lose a Samsung token when two pairings race to the same credentials file, and credential bytes can no longer leak through a pre-planted symlink at the temp path.

Detail: `upsert_samsung_token` now serializes the read→edit→temp-write→rename window through a process-wide per-path lock, writes the temp file with `create_new(true)` and (on Unix) `O_NOFOLLOW`, and uses a unique sibling name so two concurrent calls cannot clobber each other.
