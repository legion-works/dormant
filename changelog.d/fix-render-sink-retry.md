---
kind: fix
surfaces: [daemon, cli, tray, webui]
issues: [398]
---
Detail: Render stages retry construction on each show request after a connection or output lookup error. Status reports the render failure until the sink starts. A render stage without an output connector reports the configuration error without a retry.
