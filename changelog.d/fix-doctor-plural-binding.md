---
kind: fix
surfaces: []
---
`dormantctl doctor` no longer fails every display's wear-sampling probe when the active-sampling config lists more than one display. The probe compared each display's consent binding against `first_sampled_display()`, which returns nothing under a plural config, so every bound display read as a mismatch and reported a binding error even while sampling worked correctly.
