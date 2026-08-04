---
kind: fix
surfaces: [docs]
---
Linux release binaries are now built on Ubuntu 24.04 rather than 22.04, because active wear sampling links against PipeWire 1.0 headers that 22.04 does not ship. Prebuilt Linux binaries now require glibc 2.39 or newer; on an older distribution, build from source or install from the AUR, which compile against your own system libraries.
