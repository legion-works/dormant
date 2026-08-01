---
kind: capability
surfaces: [readme, chapter]
readme_bullet: "**Active wear sampling** — opt in to content-weighted OLED wear tracking from the KDE Wayland compositor."
---
User can now: opt in to content-weighted wear tracking for one KDE Wayland display.

Detail: dormant samples one compositor frame per wear tick, reduces it to a luma grid, and falls back to uniform attribution whenever capture is unavailable or stale.
