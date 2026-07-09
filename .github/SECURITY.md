# Security policy

## Supported versions

dormant is pre-1.0.

Security fixes target the current development branches only: `dev` and `master`.
Older commits, forks, and unreleased snapshots are not supported.

## Reporting a vulnerability

Use GitHub private vulnerability reporting / security advisories on this repo.
Do not file a public issue for a suspected vulnerability.

Include the affected commit, config shape, platform, and the smallest reproduction
you can share safely. Redact credentials, pairing tokens, and LAN addresses.

## Scope

dormant is a local daemon. The web UI binds to loopback only, there is no
telemetry, and there is no cloud service.

The security-sensitive surfaces are the credentials file, the IPC socket, and
network display control on the LAN.

Reports about those boundaries are in scope. Reports about third-party display,
MQTT, Home Assistant, or desktop-environment vulnerabilities belong upstream
unless dormant exposes or worsens the issue.
