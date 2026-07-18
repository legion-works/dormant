# `github.run_attempt` rerun-guard probe (T2, CI-hardening plan)

Date: 2026-07-18 · Probe PR: [#102](https://github.com/legion-works/dormant/pull/102)
(draft, closed unmerged) · Workflow: disposable `run-attempt-probe.yml` on branch
`probe/run-attempt` (deleted after capture).

## Question

Does rerunning a failed PR job increment `github.run_attempt`, execute an
`if: github.run_attempt != '1'` guard step, and leave the required check red —
for BOTH rerun shapes an operator uses (run-level "re-run failed jobs" and
job-level single-job rerun)?

## Probe design

One job, three steps:

1. `Print attempt` — always runs, echoes `run_attempt`.
2. `Guard - fail on rerun` — `if: ${{ github.run_attempt != '1' }}` → `exit 1`.
3. `Intentional first-attempt failure` — `if: ${{ github.run_attempt == '1' }}` → `exit 1`.

Note the guard compares against the STRING `'1'` — `github.run_attempt` is a
string in expression context; a numeric `> 1` comparison also works but the
probe pinned the string form actually used.

## Evidence

Run: <https://github.com/legion-works/dormant/actions/runs/29640306801>

| Attempt | Trigger | Guard step | Run conclusion |
|---|---|---|---|
| attempt 1: failure | PR open (`pull_request`) | skipped (`run_attempt == 1`) | failure (intentional step) |
| attempt 2: failure at rerun guard | `gh run rerun 29640306801 --failed` | **failure** (`run_attempt = 2`) | failure |
| attempt 3 | job-level rerun (`gh run rerun --job <job-id>`) | **failure** (`run_attempt = 3`) | failure |

Step conclusions, attempt 1 (via `/attempts/1/jobs`): Print attempt success ·
Guard skipped · Intentional failure **failure**.
Step conclusions, attempt 2 (run-level "single failed-job rerun" path, i.e.
`--failed`): Print attempt success · Guard **failure** · Intentional skipped.
Attempt 3 (job-level rerun attempt): same shape as attempt 2 — the job-level
rerun ALSO increments the run-level `run_attempt` (3) and re-evaluates the
guard.

`gh pr checks 102` after each attempt: `run-attempt-probe fail` — the check
never turned green across any rerun shape.

## Conclusions (feed T21)

1. `github.run_attempt` increments on BOTH rerun shapes (rerun-failed and
   job-level rerun attempt) — there is no rerun path that keeps `run_attempt = 1`.
2. A guard step conditioned on `run_attempt != '1'` executes on reruns and can
   force the job red — a same-SHA rerun cannot launder a required check to
   green while the guard is present.
3. The PR-facing required-check state follows the LATEST attempt; a red guard
   on attempt N keeps the check red regardless of earlier/later step outcomes.
4. T21 can therefore implement the rerun guard as designed (no alternative
   branch needed). Caveat for T21: the guard must live in every REQUIRED job
   (a guard in a non-required job does not protect the merge gate), and
   checkout-less jobs (`pr-title`) need the guard inline since they run no
   repo code.
