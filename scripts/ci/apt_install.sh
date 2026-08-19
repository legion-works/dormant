#!/usr/bin/env bash
# Install apt packages with a bounded, retrying wrapper.
#
# Every CI job that needs native libraries ran `apt-get update && apt-get
# install` directly, with no timeout and no retry. An Ubuntu archive mirror
# that stalls therefore consumes the job's entire timeout-minutes budget and
# the job is cancelled mid-apt — indistinguishable, from the checks list, from
# a build or test that genuinely ran too long. Three jobs failed that way in
# one run on 2026-08-18: policy burned 10m07s of a 10m cap, mqtt-integration
# 14m54s of 15m, and msrv 19m57s of 20m, each entirely inside apt.
#
# Raising the caps would have been the wrong response: it extends a hang
# rather than accommodating real work, and it hides the next stall behind a
# longer wall-clock. Bounding each attempt and retrying is the fix — a
# transient mirror failure costs one short attempt instead of the whole job,
# and a genuine outage still fails the job after a known, finite time.
#
# Usage: apt_install.sh <package>...
set -euo pipefail

if (( $# == 0 )); then
  printf '%s\n' 'apt_install.sh: no packages given' >&2
  exit 2
fi

# Per-attempt ceilings, overridable per job because the package sets differ by
# an order of magnitude: most jobs pull three or four small -dev packages,
# while the render job pulls ffmpeg and libavcodec-extra — 105 MB, which needs
# roughly 600 kB/s sustained to land inside the default 180s.
#
# That distinction cost a wrong diagnosis on 2026-08-19. Every attempt logged
# only "timed out", so five jobs failing together looked like a mirror outage;
# the phase counts in the log said otherwise — `apt-get update` succeeded on
# every attempt and only the 105 MB download was being cut off. A job whose
# timeout-minutes cap allows it should raise these rather than retry a download
# that cannot finish in the budget.
readonly update_timeout="${APT_UPDATE_TIMEOUT:-90}"
readonly install_timeout="${APT_INSTALL_TIMEOUT:-120}"
# Three retries after the install-first pass, so four independent tries total.
# Measured 2026-08-19: in one run seven of eight apt jobs succeeded and one
# failed, and WHICH job fails varies per run (policy on one PR, stress-ubuntu on
# another, same budgets, same minute, same mirrors). So the failure is
# per-connection variance, not a dead network or a misconfigured job — when the
# path is slow no budget rescues it, and when it is healthy 60s suffices.
#
# That makes MORE tries strictly better than LONGER ones, which is why the
# budgets above are deliberately shorter than they were: a fourth attempt buys
# another independent draw, while a longer single attempt just waits out the
# same bad connection. Every job still fails inside its timeout-minutes cap,
# which is the property that keeps a failure legible instead of a GitHub kill
# with no step diagnostic.
readonly attempts="${APT_ATTEMPTS:-3}"

# GitHub's runners point at a region-local Azure mirror. When that mirror is
# the thing stalling, retrying against it is just a slower way to fail: on
# 2026-08-19 all three attempts timed out in five jobs across two pull
# requests, ~6m50s each, entirely inside apt. Falling back to the canonical
# archive on later attempts routes around a single-mirror outage, which is what
# the retry comment below already claimed to do and did not.
readonly fallback_mirror='http://archive.ubuntu.com/ubuntu'

# Both layouts exist across runner images: 24.04 ships deb822, older releases
# the one-line format. Rewriting whichever is present is enough; a missing file
# is not an error, it just means that layout is not in use here.
switch_to_fallback_mirror() {
  local switched=0 f
  for f in /etc/apt/sources.list /etc/apt/sources.list.d/ubuntu.sources; do
    [[ -f $f ]] || continue
    if sudo sed -i -E "s#https?://[a-z0-9.-]*archive\.ubuntu\.com/ubuntu#${fallback_mirror}#g" "$f" 2>/dev/null; then
      switched=1
    fi
  done
  if (( switched == 1 )); then
    printf 'apt_install: switched to %s for the next attempt\n' "$fallback_mirror" >&2
  else
    printf '%s\n' 'apt_install: no sources file to rewrite; retrying same mirror' >&2
  fi
}

export DEBIAN_FRONTEND=noninteractive

# GitHub's runner images normally retain package lists from image creation. Use
# those first so an unrelated repository index cannot block an install that is
# already resolvable; a stale or incomplete list still falls through to update.
initial_err_log="$(mktemp)"
if timeout "$install_timeout" sudo -E apt-get install -y \
  --no-install-recommends "$@" 2>"$initial_err_log"; then
  printf 'apt_install: installed %d package(s) from existing package lists; apt-get update skipped\n' "$#"
  rm -f "$initial_err_log"
  exit 0
else
  initial_status=$?
fi
if (( initial_status == 124 )); then
  printf 'apt_install: initial apt-get install timed out after %ss; refreshing package lists\n' \
    "$install_timeout" >&2
fi
rm -f "$initial_err_log"

attempt=1
while (( attempt <= attempts )); do
  # `status` is captured in the else branch, where $? is still the condition's
  # exit status. Reading $? after the `if` block instead yields the status of
  # the `if` statement itself — always 0 — which silently disables the timeout
  # detection below and logs every failure as "exit 0".
  # apt's own stderr is captured rather than inherited: `timeout` kills the
  # child mid-write, so letting it stream means a stalled attempt often prints
  # nothing about WHY. Keeping a tail gives the next reader the mirror host and
  # the failing URL instead of a bare "timed out".
  err_log="$(mktemp)"
  if timeout "$update_timeout" sudo -E apt-get update 2>"$err_log"; then
    timed_out_phase='install'
    timed_out_budget="$install_timeout"
    if timeout "$install_timeout" sudo -E apt-get install -y \
      --no-install-recommends "$@" 2>>"$err_log"; then
      printf 'apt_install: installed %d package(s) on attempt %d\n' "$#" "$attempt"
      rm -f "$err_log"
      exit 0
    else
      status=$?
    fi
  else
    status=$?
    timed_out_phase='update'
    timed_out_budget="$update_timeout"
  fi
  # `timeout` reports 124 when it kills the child. Distinguishing a stall from
  # a package error matters: a stall is worth retrying against a possibly
  # different mirror, whereas a genuinely missing package will fail the same
  # way three times and the log should say which happened.
  if (( status == 124 )); then
    printf 'apt_install: attempt %d/%d apt-get %s timed out after %ss\n' \
      "$attempt" "$attempts" "$timed_out_phase" "$timed_out_budget" >&2
  else
    printf 'apt_install: attempt %d/%d failed (exit %d)\n' \
      "$attempt" "$attempts" "$status" >&2
  fi
  if [[ -s $err_log ]]; then
    printf '%s\n' 'apt_install: last apt stderr (tail):' >&2
    tail -n 5 "$err_log" >&2
  fi
  rm -f "$err_log"

  if (( attempt == attempts )); then
    printf '%s\n' 'apt_install: all attempts exhausted' >&2
    exit 1
  fi

  # Only after the first failure — attempt 1 should use the runner's own
  # mirror, which is normally faster than the canonical archive.
  if (( attempt == 1 )); then
    switch_to_fallback_mirror
  fi

  # Linear backoff. An archive stall is usually brief and often resolves on a
  # different mirror, so waiting long buys little; the point is to not hammer.
  sleep $(( attempt * 10 ))
  attempt=$(( attempt + 1 ))
done
