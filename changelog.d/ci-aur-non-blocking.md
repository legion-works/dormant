---
kind: fix
surfaces: [docs]
---
A failed AUR publish no longer marks an otherwise-successful release run as failed. The AUR's git endpoint goes offline for maintenance and restricts pushes during upstream security incidents, neither of which reflects on the release itself. The published AUR version is verified out of band rather than inferred from the job's exit code.
