---
kind: fix
surfaces: [displays]
issues: []
---
Detail: A DDC/CI power-off is confirmed when the panel reads back any MCCS off state (standby, suspend, soft off or hard off), not only the exact value written. An AOC AG326UZD on HDMI reports soft off (0x04) after a hard-off (0x05) write; dormant counted every such blank as failed, retried it and reported blank failures while the panel was already dark. A readback that still reports on, or a value outside the MCCS range, still fails the blank.
