# Documentation standard

What each document in this repository is for, and the gates that keep them from
drifting behind the code.

## Release notes are compiled, not written

Release notes written at tag time describe what changed in the code, because
that is what a diff shows. A release that deletes more than it adds then reads
as subtraction even when it ships a major capability.

So the user-facing sentence is written in the pull request that introduces the
change, while the person writing it still has the change in their head. Release
assembly compiles those sentences; nobody authors release notes from a diff.

```
PR          add changelog.d/<slug>.md
CI          fragment present and schema-valid
release     scripts/release-prep.py compiles fragments into the changelog entry
            and refuses to proceed if a declared surface was not updated
            --write inserts the entry and verifies write integrity
            --delete-fragments re-checks coverage before consuming fragments
```

CI cannot judge whether a change is user-facing. It can judge whether a file
exists and parses. The judgement therefore belongs to the author, who the schema
forces to answer the question; the gates stay mechanical.

This is [towncrier](https://towncrier.readthedocs.io/)'s model, used in CPython,
pytest, and GitLab. A small script is enough here.

## Changelog fragments

One file per pull request: `changelog.d/<branch-slug>.md`. Use the branch slug —
the PR number does not exist yet when the fragment is written.

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

| Field | Rule |
|---|---|
| `kind` | `capability` — something a user can newly do · `improvement` — a better version of something they could already do · `fix` — a defect · `breaking` — anything requiring operator action on upgrade |
| `surfaces` | Which documents this obliges: `readme`, `chapter`, or both. Empty is allowed only for `fix` and `improvement`. A `capability` must declare at least `readme`. |
| `readme_bullet` | Required when `surfaces` includes `readme`. The exact bullet text as it will appear in the README's "What it does" list; release assembly greps for this string. |
| `User can now:` | Required for `capability`. One sentence, user verb first, no mechanism. Mechanism goes in `Detail:`. |
| `Detail:` | Optional, one to three lines, for the `Added`/`Changed`/`Fixed` bucket. |
| migration note | Required for `breaking`: what the operator must do, in the imperative. |

Fragments are deleted in the release commit. The changelog is the archive.

A refactor, test, CI, or docs-only change needs no fragment — the gate fires
only on `feat:` and `fix:` titles.

## README

Order: **problem → capabilities → proof → install → depth.** A reader learns
what `dormant` does for them before any mechanism appears.

```
# dormant
<one sentence: the problem, and the outcome>

## What it does
One bullet per shipped capability. User verb first, mechanism last or absent.
Every capability fragment lands exactly one bullet here.

## Quick start
Install, minimal config, first run, and the one command that verifies it.

## Why not just DPMS?
The differentiator. Mechanism tables belong here, below capability.

## Documentation
Link to the book, one line per binary.

## Status
Platform matrix, roadmap link.
```

A capability is not shipped until it has a "What it does" bullet. Release
assembly enforces this: the fragment carries the bullet text and the release
refuses to proceed if the string is absent from `README.md`.

## Changelog

Keep the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) buckets for
the detail record. Two sections sit above them:

```markdown
## [0.7.0] - 2026-07-26

### Breaking
- `coordination.enabled` and eleven sibling keys were removed. Delete them from
  `config.toml` and run `dormantctl validate` before upgrading — strict
  unknown-key validation rejects a config that still carries them.

### Highlights
**Soft KVM — share one monitor between two machines.** Press a hotkey on either
machine to pull the panel to it, or use `dormantctl switch`, the tray, or the
web dashboard. An opt-in mode follows local input activity. See
[Multi-machine](src/multi-machine.md).

### Added
### Changed
### Fixed
### Removed
### Security
```

- `Breaking` comes first whenever present, however small. Loudness is position,
  not adjectives.
- `Highlights` is required whenever the release contains a `capability`
  fragment: one short paragraph each, what the user can now do, then the chapter
  link.
- `Removed` never leads an entry that has highlights. Section order guarantees
  it.

Breaking means anything requiring operator action on upgrade: config keys, CLI
flags, service files, a `config_version` bump, or a changed default that alters
behaviour.

`cargo-dist` derives the GitHub Release body from this file, so the published
notes inherit the same order. There is no second artifact to keep in sync.

## Book chapters

Every feature chapter under `docs/src/` opens with three blocks before any
reference material:

```markdown
# <Feature>

**What this gives you.** Two or three sentences. User outcome, no mechanism.

**When to use it.** The scenario, and when not to — name the nearest
alternative and why you would choose it instead.

**Quick setup.** The smallest working config fragment, and the one command that
verifies it (usually a `dormantctl doctor` probe).

---

Full config reference · behaviour details · failure modes and troubleshooting
```

A chapter can satisfy every rule above while the README and changelog stay
silent, which is why announcement is declared in the fragment rather than left
to each document. The chapter, the README bullet, and the highlight are three
derived obligations of one declared fact.

## Gates

**On every pull request** — `scripts/ci/check_changelog_fragment.py` fails when:

- the title type is `feat` or `fix` and no new `changelog.d/` file is added
- `kind: capability` has no `User can now:` line, or an empty `surfaces` list
- `surfaces` includes `readme` and `readme_bullet` is missing or empty
- `kind: breaking` has no migration sentence
- the front matter does not parse, or `kind` is not one of the four values

The `skip-changelog` label bypasses it. The pull request description must say
why; a pattern of use is a review trigger.

**At release** — `scripts/release-prep.py` compiles fragments into the changelog
entry and refuses to proceed when:

- a fragment declares `surfaces: [readme]` and its `readme_bullet` is absent
  from `README.md`
- a fragment declares `surfaces: [chapter]` and no file under `docs/src/`
  changed since the previous tag
- a `capability` fragment exists and the compiled entry has no `Highlights`
- `--write` inserts a new version section and verifies write integrity
- `--delete-fragments` refuses to consume fragments when any emitted bullet or
  highlight is absent from the newest changelog section
- `issues: [123]` and `prs: [456]` append issue and pull-request citations to
  compiled entries

### Verifying the gates

Both need to be proven to fail, not only to pass:

- Open a `feat:` pull request with no fragment; CI must fail. Add the fragment;
  it must pass.
- Run `release-prep.py` against a fragment set containing one `capability` with
  `surfaces: [readme]` and an untouched README; it must refuse.
- Bare `release-prep.py --check` validates surfaces only, never coverage,
  because fragments legitimately outrun the changelog between releases — it
  runs in the release pipeline's `release-changelog-gate` job, where the
  changelog section does not exist yet. Do not make coverage unconditional
  there; it would fail every release before the section is written.
- Run `release-prep.py --check --changelog CHANGELOG.md` to verify coverage
  before release. The release sequence is `--write` → `--delete-fragments`;
  `--write` verifies the exact inserted entry, and deletion verifies coverage
  before consuming fragments.
