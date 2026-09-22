---
kind: fix
surfaces: [web]
issues: [265]
---
Detail: the Web UI config-apply path failed on Windows with `fsync failed: Access is denied. (os error 5)`. `sync_file` reopened the temp config read-only before `sync_all`, but `sync_all` is `FlushFileBuffers` on Windows, which requires the `GENERIC_WRITE` access right. The handle is now opened for write.
