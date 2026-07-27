---
kind: fix
surfaces: []
---

Two operator-safety fixes: `dormantctl blank` now performs a soft blank (the
same render/controller ladder a vacant rule would walk) and hard power-off
moved behind an explicit `--hard` flag with confirmation (`--yes` for
scripts; web and tray force-blank surfaces stay hard and explicit); and
hardware operations (`doctor exercise`, emergency wake) are now fenced by
generation — a reload waits for in-flight operations (cancelling them
cooperatively after a bound, rejecting the reload rather than tearing down a
generation that still holds hardware), a panicking exercise restores its
rule pause and wakes the panel, and a stale operation's completion can never
mutate the generation that replaced it.
