---
kind: fix
surfaces: [daemon, docs]
issues: []
---
Detail: The systemd user unit starts the daemon only when the user manager has `WAYLAND_DISPLAY` or `DISPLAY`. A `systemctl --user enable --now` from SSH or a text console before login used to start a daemon that could not draw render stages, read idle time or run compositor hooks until it restarted; render failures then made the blank ladder go straight to a DDC/CI power-off. Such a start is now skipped, and the unit starts with the desktop session. Copy the updated unit to `~/.config/systemd/user/` and run `systemctl --user daemon-reload` to apply it.
