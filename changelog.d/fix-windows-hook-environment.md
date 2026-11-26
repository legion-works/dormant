---
kind: fix
surfaces: [daemon]
issues: [417]
---
Detail: Windows hooks use a system `PATH` and receive Windows environment variables such as `SystemRoot` and `ComSpec` when the daemon has them.
