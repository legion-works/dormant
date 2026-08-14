---
kind: fix
surfaces: []
issues: []
---
Detail: The tray's build script resolved its icon assets against a path baked in at compile time, so a build in one git worktree could read another worktree's files — or fail outright once that checkout was gone.
