---
kind: capability
surfaces: [readme, chapter]
readme_bullet: "**MQTT state publishing** — opt-in retained state + Home Assistant discovery for sensors, zones, and displays"
chapter: mqtt-publishing.md
---
User can now: mirror dormant's live state into Home Assistant with two config
lines — sensors, zones, and display phases appear as auto-discovered HA
entities over MQTT, with retained state, per-sensor availability, and a
last-will `offline` marker when the daemon dies.
