---
kind: fix
surfaces: []
---
Audit and correct the book chapters against the source code: four documented
claims that contradicted the current code, six gaps filled, and seven feature
chapters brought into compliance with DOCS-STANDARD.md opening blocks.

Detail: ownership section now reflects the verified-pull-marks-owned path
(issue #139); hook blocking defaults are now phase-dependent; `/api/push` is
surfaced in network-surface and CSRF-guard lists; `coord_ownership_gain_deferred`
is documented; D6 probe and write-verification retries are noted; the
out-of-set poll-read rule is tightened to transport-failure semantics; and the
"Switch to here" / "Send to peer" web affordances are described.
