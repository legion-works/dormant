---
kind: improvement
surfaces: []
issues: []
---
Detail: CI's package installer now falls back to the canonical Ubuntu archive after its first failed attempt, and prints the tail of apt's own error output. A stalled region-local mirror previously failed every retry against the same unreachable host and reported only that the attempt timed out.
