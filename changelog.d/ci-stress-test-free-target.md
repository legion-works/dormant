---
kind: improvement
surfaces: []
issues: []
---
Detail: The changed-test stress selector no longer fails when a selected library or binary target contains no test code. An empty integration-test target is still treated as a selection error, since such a target exists only to hold tests.
