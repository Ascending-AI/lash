#!/usr/bin/env bash
set -euo pipefail

# Guarantee a settled CI conclusion for an exact head commit.
#
# GitHub intermittently creates no Actions check suite for a branch push even
# though the push itself was delivered (FIG-1824). The head then sits with no
# CI at all, and every watcher that gates on a run conclusion waits forever.
# The recovery has always been a manual `gh workflow run CI --ref <branch>`;
# this script is that recovery made deterministic and machine-readable.
#
# Usage:
#   scripts/ci_ensure_run.sh <branch> <head-sha> [options]
#
# Options:
#   --wait-seconds N        how long to wait for a naturally-triggered run
#                           before dispatching (default 600)
#   --poll-seconds N        interval between polls (default 20)
#   --run-timeout N         how long to wait for the located run to conclude
#                           (default 5400)
#   --workflow NAME         workflow name (default CI)
#   --repo OWNER/NAME       repository (default: the checkout's repo)
#   --accept-dispatch-run   treat a green workflow_dispatch recovery run as a
#                           full pass (exit 0 instead of 3). Read the warning
#                           below before passing this.
#   --dry-run               print what would happen and exit without contacting
#                           the API
#
# ## Exit codes — read this before gating anything on them
#
#   0  a run for <head-sha> concluded success AND that run was triggered by a
#      real repository event (pull_request / push / merge_group), so the full
#      gate set applied.
#   3  a run for <head-sha> concluded success but it was a workflow_dispatch
#      RECOVERY run, which is materially weaker than the run it replaced —
#      see below. Never treat 3 as a pass without deciding you can afford the
#      waived gates; pass --accept-dispatch-run to collapse it into 0.
#   1  the run settled on any other conclusion, or no run for this head could
#      be located within the timeouts.
#   2  usage error, the dispatch itself failed, or the API was unreadable for
#      long enough that no honest answer exists.
#
# ## Why a dispatch run is not equivalent to the run it replaces
#
# ci.yml conditions real gates off `github.event_name`, so a workflow_dispatch
# run of a PR head SKIPS:
#
#   * `Check versioned surface bumps`  (if: event_name != 'workflow_dispatch')
#     — the gate that fails a PR changing a versioned surface without bumping
#     it (scripts/check_version_bumps.py, scripts/versioned-surfaces.toml).
# It also builds a different tree: a `pull_request` run carries
# `refs/pull/<n>/merge` (the merged result), a dispatch carries
# `refs/heads/<branch>` (the branch tip alone). A green dispatch therefore
# proves less, about a different tree, than the green it stands in for. That
# is the entire reason the dispatch path has its own exit code.
#
# Also note: ci.yml's concurrency group makes a branch dispatch and a
# late-arriving natural run cancel each other. If the dropped event shows up
# after this script dispatches, whichever run it bound may settle `cancelled`
# — reported as completed:cancelled, exit 1. Fail-safe, but re-run rather than
# treating it as a real red.
#
# ## Status lines
#
# One line per state change on stdout, whitespace-separated; everything else —
# progress chatter, errors — goes to stderr, so a watcher can consume stdout
# alone.
#
#   CI_ENSURE <sha> waiting
#   CI_ENSURE <sha> found run=<id> status=<status> event=<event>
#   CI_ENSURE <sha> dispatched
#   CI_ENSURE <sha> completed:<conclusion> event=<event>
#   CI_ENSURE <sha> missing reason=<reason>
#   CI_ENSURE <sha> api-failure
#   CI_ENSURE <sha> dry-run
#
# A watcher should match on the stdout line as well as the exit code: the one
# exit 0 not backed by a settled run is `--dry-run`, which an operator passes
# deliberately and a watcher must never pass.

branch=""
head_sha=""
wait_seconds=600
poll_seconds=20
run_timeout=5400
workflow="CI"
repo=""
dry_run=0
accept_dispatch=0

# Per-call wall-clock bound on every gh invocation. Every deadline in this
# script is checked *between* API calls, and gh has no default request
# timeout, so without this a blackholed socket hangs forever inside a call and
# no deadline is ever reached (the one genuinely unbounded wait).
gh_call_timeout=60

# Consecutive API failures tolerated while polling a located run before we
# stop calling an unreadable API "still running".
max_consecutive_api_failures=5

die_usage() {
  echo "$1" >&2
  echo "usage: ci_ensure_run.sh <branch> <head-sha> [--wait-seconds N]" \
    "[--poll-seconds N] [--run-timeout N] [--workflow NAME]" \
    "[--repo OWNER/NAME] [--accept-dispatch-run] [--dry-run]" >&2
  exit 2
}

require_int() {
  # $1 flag name, $2 value
  case "${2-}" in
    ''|*[!0-9]*) die_usage "$1 requires a non-negative integer, got: ${2-}" ;;
  esac
}

while [ $# -gt 0 ]; do
  case "$1" in
    --wait-seconds)
      [ $# -ge 2 ] || die_usage "$1 requires a value"
      require_int "$1" "$2"
      wait_seconds="$2"
      shift 2
      ;;
    --poll-seconds)
      [ $# -ge 2 ] || die_usage "$1 requires a value"
      require_int "$1" "$2"
      poll_seconds="$2"
      shift 2
      ;;
    --run-timeout)
      [ $# -ge 2 ] || die_usage "$1 requires a value"
      require_int "$1" "$2"
      run_timeout="$2"
      shift 2
      ;;
    --workflow)
      [ $# -ge 2 ] || die_usage "--workflow requires a value"
      workflow="$2"
      shift 2
      ;;
    --repo)
      [ $# -ge 2 ] || die_usage "--repo requires a value"
      repo="$2"
      shift 2
      ;;
    --accept-dispatch-run)
      accept_dispatch=1
      shift
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    -h|--help)
      die_usage "ci_ensure_run.sh"
      ;;
    -*)
      die_usage "unknown option: $1"
      ;;
    *)
      if [ -z "$branch" ]; then
        branch="$1"
      elif [ -z "$head_sha" ]; then
        head_sha="$1"
      else
        die_usage "unexpected argument: $1"
      fi
      shift
      ;;
  esac
done

[ -n "$branch" ] || die_usage "missing <branch>"
[ -n "$head_sha" ] || die_usage "missing <head-sha>"

case "$head_sha" in
  *[!0-9a-fA-F]*|"") die_usage "head-sha must be hexadecimal, got: $head_sha" ;;
esac
if [ "${#head_sha}" -ne 40 ]; then
  die_usage "head-sha must be a full 40-character SHA, got ${#head_sha} chars: $head_sha"
fi
# GitHub reports lowercase SHAs and the API's head_sha filter is
# case-sensitive. An uppercase argument would otherwise match nothing: a full
# wait window, a real dispatch, and a false red on a head that was green.
head_sha="${head_sha,,}"
[ "$poll_seconds" -gt 0 ] || die_usage "--poll-seconds must be positive"

status_line() {
  echo "CI_ENSURE $head_sha $*"
}

note() {
  echo "ci_ensure_run: $*" >&2
}

if [ "$dry_run" -eq 1 ]; then
  note "dry run: no API calls, no dispatch"
  {
    printf 'would look the head up directly, every %ss for up to %ss:\n' \
      "$poll_seconds" "$wait_seconds"
    printf '  gh api "repos/<owner>/<repo>/actions/runs?head_sha=%s&per_page=100"\n' \
      "$head_sha"
    printf '    (exact head match, no listing window; filtered to workflow %s on branch %s)\n' \
      "$workflow" "$branch"
    printf 'if that returns no run, once:\n'
    printf '  gh workflow run %s %s--ref %s\n' \
      "$workflow" "${repo:+--repo $repo }" "$branch"
    printf 'then poll the newest matching run for up to %ss:\n' "$run_timeout"
    printf '  gh run view <id> %s--json status,conclusion,event\n' \
      "${repo:+--repo $repo }"
    printf 'a green workflow_dispatch run would exit %s, not 0\n' \
      "$([ "$accept_dispatch" -eq 1 ] && echo 0 || echo 3)"
  } >&2
  status_line "dry-run"
  exit 0
fi

command -v gh >/dev/null 2>&1 || {
  note "gh is not on PATH"
  exit 2
}

# `timeout` is coreutils and present everywhere this runs, but degrade rather
# than refuse if it is not: the bound is defense in depth, not correctness.
gh_timeout_prefix=()
if command -v timeout >/dev/null 2>&1; then
  gh_timeout_prefix=(timeout "$gh_call_timeout")
else
  note "timeout(1) not found; gh calls are not individually time-bounded"
fi

gh_args=()
if [ -n "$repo" ]; then
  gh_args=(--repo "$repo")
fi

# The runs API is keyed by owner/repo in the path, so resolve the slug once.
if [ -z "$repo" ]; then
  if ! repo="$("${gh_timeout_prefix[@]}" gh repo view --json nameWithOwner --jq .nameWithOwner)"; then
    note "could not determine the repository; pass --repo OWNER/NAME"
    exit 2
  fi
fi

# GitHub's clock, not ours. The dispatch-locating filter compares against the
# API's `created_at`, so a local clock leading GitHub's would filter out this
# script's own dispatched run. Falls back to a generous local-clock skew
# budget if the header is unavailable.
github_epoch() {
  local date_header=""
  date_header="$(
    "${gh_timeout_prefix[@]}" gh api -i rate_limit 2>/dev/null \
      | tr -d '\r' \
      | sed -n 's/^[Dd]ate: //p' \
      | head -1
  )" || true
  if [ -n "$date_header" ] && date -u -d "$date_header" +%s >/dev/null 2>&1; then
    date -u -d "$date_header" +%s
    return 0
  fi
  return 1
}

# All runs for this exact head, newest first, as
# "<id> <status> <conclusion> <event> <created_at>". The head_sha query
# parameter is an exact server-side filter with no listing window, so a head
# older than the last N runs on the branch is still found — a `gh run list
# --limit N` window would silently report "no run" for it and dispatch.
list_runs_for_head() {
  "${gh_timeout_prefix[@]}" gh api \
    "repos/$repo/actions/runs?head_sha=$head_sha&per_page=100" \
    --jq "[ .workflow_runs[]
            | select(.name == \"$workflow\")
            | select(.head_branch == \"$branch\") ]
          | sort_by(.created_at)
          | reverse
          | .[]
          | \"\(.id) \(.status) \(.conclusion) \(.event) \(.created_at)\""
}

# 1 when the most recent listing attempt succeeded. Checked immediately before
# dispatching: one success at t=0 must not authorise a dispatch at t=600 after
# the API went down in between.
last_list_ok=0
# 1 once any listing has succeeded, i.e. we have ever had a readable answer.
api_ever_ok=0

found_run=""
found_event=""

# Newest run for this head created at or after $1 (epoch seconds; 0 for any),
# into the globals `found_run` ("<id> <status> <conclusion>") and
# `found_event`.
#
# The result travels in globals rather than on stdout on purpose: a command
# substitution would run this in a subshell, and the flags that decide whether
# dispatching is safe would never reach the caller.
#
# Returns 0 found, 1 no matching run, 2 the listing call itself failed.
find_run() {
  local since_epoch="$1"
  local raw="" rc=0 id status conclusion event created
  found_run=""
  found_event=""
  raw="$(list_runs_for_head)" || rc=$?
  if [ "$rc" -ne 0 ]; then
    last_list_ok=0
    if [ "$rc" -eq 124 ]; then
      note "runs listing timed out after ${gh_call_timeout}s; retrying"
    else
      note "runs listing failed (exit $rc); retrying"
    fi
    return 2
  fi
  last_list_ok=1
  api_ever_ok=1
  [ -n "$raw" ] || return 1
  while read -r id status conclusion event created; do
    [ -n "$id" ] || continue
    if [ "$since_epoch" -gt 0 ]; then
      local created_epoch
      created_epoch="$(date -u -d "$created" +%s 2>/dev/null || echo 0)"
      [ "$created_epoch" -ge "$since_epoch" ] || continue
    fi
    found_run="$id $status $conclusion"
    found_event="$event"
    return 0
  done <<<"$raw"
  return 1
}

# Poll a located run to completion. Sets `settled_conclusion` and
# `settled_event`; returns 0 settled, 1 timed out / unreadable conclusion,
# 2 the API stayed unreadable.
settled_conclusion=""
settled_event=""
poll_to_conclusion() {
  local run_id="$1"
  local deadline=$(($(date +%s) + run_timeout))
  local raw status conclusion event rc failures=0 null_seen=0
  settled_conclusion=""
  settled_event=""
  while :; do
    rc=0
    raw="$(
      "${gh_timeout_prefix[@]}" gh run view "$run_id" "${gh_args[@]}" \
        --json status,conclusion,event \
        --jq '"\(.status) \(.conclusion) \(.event)"'
    )" || rc=$?
    if [ "$rc" -ne 0 ] || [ -z "$raw" ]; then
      # An unreadable API is not "still running" — say so, and stop calling it
      # progress once it is clearly not transient.
      failures=$((failures + 1))
      note "gh run view failed (attempt $failures/$max_consecutive_api_failures, exit $rc)"
      if [ "$failures" -ge "$max_consecutive_api_failures" ]; then
        return 2
      fi
      sleep "$poll_seconds"
      continue
    fi
    failures=0
    read -r status conclusion event <<<"$raw"
    if [ "$status" = "completed" ]; then
      if [ "$conclusion" = "null" ] || [ -z "$conclusion" ]; then
        # A run reading completed with no conclusion yet is a finalizing race;
        # give it exactly one more poll before calling it unreadable.
        if [ "$null_seen" -eq 0 ]; then
          null_seen=1
          note "run $run_id completed with a null conclusion; re-reading once"
          sleep "$poll_seconds"
          continue
        fi
        return 1
      fi
      settled_conclusion="$conclusion"
      settled_event="$event"
      return 0
    fi
    null_seen=0
    if [ "$(date +%s)" -ge "$deadline" ]; then
      note "run $run_id still $status after ${run_timeout}s"
      return 1
    fi
    note "run $run_id status=$status; waiting ${poll_seconds}s"
    sleep "$poll_seconds"
  done
}

settle() {
  local run_id="$1"
  local rc=0
  poll_to_conclusion "$run_id" || rc=$?
  if [ "$rc" -eq 2 ]; then
    status_line "api-failure"
    exit 2
  fi
  if [ "$rc" -ne 0 ]; then
    status_line "missing reason=unsettled"
    exit 1
  fi
  status_line "completed:$settled_conclusion event=$settled_event"
  if [ "$settled_conclusion" != "success" ]; then
    exit 1
  fi
  if [ "$settled_event" = "workflow_dispatch" ] && [ "$accept_dispatch" -eq 0 ]; then
    # Green, but on the weaker gate set — the caller decides, not this script.
    note "run $run_id is a workflow_dispatch recovery run: 'Check versioned" \
      "surface bumps' did not run, and it built refs/heads/$branch rather" \
      "than the PR merge tree." \
      "Exiting 3; pass --accept-dispatch-run to treat this as a full pass."
    exit 3
  fi
  exit 0
}

# 1. Wait a bounded window for a run that GitHub created on its own. A run
#    that already exists but is still in progress needs no dispatch — poll it.
wait_deadline=$(($(date +%s) + wait_seconds))
announced_waiting=0
while :; do
  if find_run 0; then
    read -r run_id run_status _ <<<"$found_run"
    status_line "found run=$run_id status=$run_status event=$found_event"
    settle "$run_id"
  fi
  if [ "$(date +%s)" -ge "$wait_deadline" ]; then
    break
  fi
  if [ "$announced_waiting" -eq 0 ]; then
    status_line "waiting"
    announced_waiting=1
  fi
  sleep "$poll_seconds"
done

# 2. No run exists for this head: GitHub dropped the event. Dispatch once,
#    then locate *that* run — same head, created at or after the dispatch —
#    rather than any older run on the branch.
if [ "$api_ever_ok" -eq 0 ] || [ "$last_list_ok" -eq 0 ]; then
  note "the runs listing was unreadable at the moment of decision;" \
    "refusing to dispatch on an unread state"
  status_line "api-failure"
  exit 2
fi

note "no ${workflow} run for $head_sha after ${wait_seconds}s; dispatching"
if ! dispatch_epoch="$(github_epoch)"; then
  # No GitHub clock in hand: fall back to the local one with a skew budget
  # wide enough to survive an unsynchronised host. Binding a slightly older
  # run for this same head is harmless — its event is reported either way.
  dispatch_epoch=$(($(date +%s) - 300))
  note "could not read GitHub's clock; using local time with a 300s skew budget"
fi
if ! "${gh_timeout_prefix[@]}" gh workflow run "$workflow" "${gh_args[@]}" --ref "$branch" >&2; then
  note "dispatch failed"
  exit 2
fi
status_line "dispatched"

# The dispatched run takes a few seconds to materialise, and it must carry the
# expected head: if the branch has moved on, the dispatch built a different
# commit and this head is still unvalidated.
locate_deadline=$(($(date +%s) + 300))
run_id=""
while :; do
  if find_run "$dispatch_epoch"; then
    read -r run_id run_status _ <<<"$found_run"
    break
  fi
  if [ "$(date +%s)" -ge "$locate_deadline" ]; then
    note "dispatched run for $head_sha never appeared (branch moved off this head?)"
    status_line "missing reason=dispatch-not-located"
    exit 1
  fi
  sleep "$poll_seconds"
done

status_line "found run=$run_id status=$run_status event=$found_event"
settle "$run_id"
