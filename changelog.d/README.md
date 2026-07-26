# Changelog fragments

One file per pull request, compiled into the changelog at release time. This
directory is a queue, not an archive — fragments are deleted in the release
commit.

Every `feat:` or `fix:` pull request needs one. CI enforces it. The full
standard, including the README and chapter rules these fragments drive, is in
[`docs/DOCS-STANDARD.md`](../docs/DOCS-STANDARD.md).

Write the fragment while the change is fresh. Release notes assembled from a
diff describe what changed in the code, not what a user can now do — and a
release that deletes more than it adds then reads as subtraction.

## Naming

`changelog.d/<branch-slug>.md`. Use the branch slug, not the pull request
number; the number does not exist yet.

## Template

```markdown
---
kind: capability
surfaces: [readme, chapter]
readme_bullet: "**Soft KVM** — two machines, one monitor; pull the panel by hotkey, CLI, tray, or web."
---
User can now: share one physical monitor between two machines and pull the
panel to whichever one they are using.

Detail: either machine writes the shared display's input code over its own DDC
bus. Ownership is observed from VCP `0x60` polling rather than negotiated, so
there is no pairing, no peer connection, and no network protocol.
```

## Fields

- **`kind`** — `capability` (something a user can newly do) · `improvement` (a
  better version of something they could already do) · `fix` (a defect) ·
  `breaking` (anything requiring operator action on upgrade).
- **`surfaces`** — which documents this obliges: `readme`, `chapter`, or both.
  Empty is allowed only for `fix` and `improvement`. A `capability` must declare
  at least `readme`.
- **`readme_bullet`** — required when `surfaces` includes `readme`. The exact
  bullet text as it will appear in the README's "What it does" list; release
  assembly greps the README for this string and refuses to tag without it.
- **`User can now:`** — required for `capability`. One sentence, user verb
  first, no mechanism. Mechanism goes in `Detail:`.
- **`Detail:`** — optional, one to three lines, for the `Added`/`Changed`/`Fixed`
  bucket.
- A `breaking` fragment needs a migration sentence in the imperative: what the
  operator must do.

## Nothing user-visible?

Use `kind: fix` or `kind: improvement` with an empty `surfaces` list. A
refactor, test, CI, or docs-only change needs no fragment at all — the gate
fires only on `feat:` and `fix:` titles.
