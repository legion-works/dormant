---
kind: capability
surfaces: [web, docs, cli]
issues: []
---
User can now: catch a misclick that flips a zone's `unavailable_policy` to `"absent"` (would blank on a sensor outage, fail-unsafe) before the change is applied; see one-line guidance and an example placeholder on every creatable config field; and have a newly-created sensor joined into an existing zone in the same apply; and script `dormantctl status` as JSON.

Detail: Adding a sensor / display / rule used to drop the operator into raw config fields with no guidance and no path back to a zone — the create form now leads with closed-set selects for sensor type / kind and zone mode / unavailable_policy, per-field one-line help sourced from `docs/src/configuration.md`, and an optional `Add to zone` affordance whose submit emits the `CreateEntity` patch plus a Set patch appending the new sensor id to `zones.<id>.members` (the server already accepted that whole-array set). The apply bar now requires explicit consent when a pending edit flips a deliberate fail-safe default — the first such setting is `zones.<id>.unavailable_policy = "absent"`, named with its consequence; `dormantctl status --json` is now mentioned in the quickstart and in the troubleshooting recipe that already uses `dormantctl status`.
