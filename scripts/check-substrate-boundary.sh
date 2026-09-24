#!/usr/bin/env bash
set -euo pipefail

# Clock-rule scope exemptions outside lash-core (inventory re-derived at 2fe260e7e):
# - No lash-core dependency: lash-llm-transport, lash-http-transport, lash-plugin-mcp.
# - Clock implementations: lash-sim/src/clock.rs:64 and
#   lash-core-ids/src/test_clock.rs:35,39.
# - Benchmark harness (24 sites):
#   lash-perf/src/runtime_perf/measurement/store_hardening.rs:229;
#   lash-perf/src/runtime_perf/measurement/provider_scenarios.rs:62,213;
#   lash-perf/src/runtime_perf/harness/observation.rs:98;
#   lash-perf/src/runtime_perf/measurement/checkpoint_curve.rs:360;
#   lash-perf/src/runtime_perf/measurement/queued_work.rs:263,773;
#   lash-perf/src/runtime_perf/providers/tools.rs:458,473,477,512,761,853,912;
#   lash-perf/src/runtime_perf/measurement/process_stress.rs:289;
#   lash-perf/src/runtime_perf/measurement/contention.rs:110,471,636,646,723;
#   lash-perf/src/runtime_perf/measurement/high_traffic.rs:440;
#   lash-perf/src/runtime_perf/measurement/checkpoint.rs:99,363;
#   lash-perf/src/runtime_perf/measurement/live_replay.rs:231.
# - Engine-owned pacing: lash-restate/src/process/workflow.rs:426.
# - Store-local retry: lash-postgres-store/src/postgres/attachments.rs:108,
#   lash-postgres-store/src/bin/postgres-await-event-helper.rs:84,
#   lash-sqlite-store/src/bin/sqlite-await-event-helper.rs:85.
# - Other deliberately out-of-scope sites: lash/src/session.rs:983;
#   lash-provider-openai/src/codex/ws_testing.rs:286,468;
#   lash-protocol-rlm/src/executor/host_bridge.rs:1040;
#   lash-sim/src/backend_contention.rs:536;
#   lash-sim/src/runner/generated_world.rs:551,903.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

clock_forbidden='tokio::time::(sleep|sleep_until|interval)|tokio::task::yield_now|use[[:space:]]+tokio::time::\{[^}]*(sleep|sleep_until|interval)'
containment_forbidden='(^|[^[:alnum:]_])(NativeQueuedWork|NoQueuedWork|NativeProcessWork|NativeProcessAwaiter|NativeSubstrateSetup|NativeSubstrateSlot|WakeDeliveryDriver)([^[:alnum:]_]|$)'
fallback_forbidden='ProcessAwaiter::polling|Option[[:space:]]*<[[:space:]]*Arc[[:space:]]*<[[:space:]]*dyn[[:space:]]+(QueuedWorkSubstrate|ProcessWorkSubstrate)[[:space:]]*>[[:space:]]*>|Option[[:space:]]*<[[:space:]]*(ProcessWorkDriver|QueuedWorkDriver)[[:space:]]*>'
capability_names='replay_ownership|journal_addressing|durable_workflow_controller|allows_process_lifetime_completion_keys|turn_control_participation|runtime_effect_failure_disposition'
capability_forbidden="fn[[:space:]]+(${capability_names})([^[:alnum:]_]|$)|\.(${capability_names})[[:space:]]*\(|(^|[^[:alnum:]_])(${capability_names})[[:space:]]*:"
test_path_regex='^crates/lash-conformance/|(^|/)(tests?|testing|[a-z_]*_tests)(/|\.rs$)'
containment_test_path_regex='^crates/lash-conformance/|(^|/)(tests?|testing|[a-z_]*_tests)(/|\.rs$)|_tests\.rs$'

tmp_dir="$(mktemp -d)"
trap 'rm -rf -- "$tmp_dir"' EXIT
failed=0

search_rust() {
  local pattern=$1
  shift
  if command -v rg >/dev/null 2>&1; then
    rg -n --glob '*.rs' "$pattern" "$@"
  else
    # Portable fallback when ripgrep is unavailable: grep -E over the same
    # Rust source trees. -r recurses like rg's directory walk, -n prints line
    # numbers, and --include keeps Rule 4 code-shaped by ignoring non-Rust files.
    grep -rEn --include='*.rs' "$pattern" "$@"
  fi
}

capture_search() {
  local label=$1
  local pattern=$2
  local output=$3
  shift 3

  if search_rust "$pattern" "$@" >"$output"; then
    return
  else
    search_status=$?
    if [[ $search_status -ne 1 ]]; then
      echo "substrate boundary check failed: $label search exited $search_status" >&2
      failed=1
    fi
    : >"$output"
  fi
}

line_in_test_region() {
  local file=$1 line=$2
  # A production file can contain any number of `#[cfg(test)]` modules, closed
  # ones with production code resuming after them and a run of them at the
  # bottom alike. Each module's region runs from its attribute to the closing
  # brace rustfmt puts at the attribute's own indentation, so the regions are
  # read off directly instead of guessing that the last attribute opens the
  # only bottom region -- a guess that read the middle module of three as
  # production code.
  awk -v target="$line" '
    { lines[NR] = $0 }
    END {
      for (i = 1; i <= NR; i++) {
        if (lines[i] !~ /^[[:space:]]*#\[cfg\(test\)\]/) {
          continue
        }
        match(lines[i], /^[[:space:]]*/)
        indent = RLENGTH
        next_line = i + 1
        while (next_line <= NR && lines[next_line] ~ /^[[:space:]]*$/) {
          next_line++
        }
        if (next_line > NR || lines[next_line] !~ /^[[:space:]]*mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*\{[[:space:]]*$/) {
          # Not a `#[cfg(test)] mod ... {` block: nothing to bound, so the
          # attribute exempts nothing.
          continue
        }
        closer = ""
        for (pad = 0; pad < indent; pad++) {
          closer = closer " "
        }
        closer = closer "}"
        end = NR
        for (j = next_line + 1; j <= NR; j++) {
          if (lines[j] == closer) {
            end = j
            break
          }
        }
        if (target >= i && target <= end) {
          print "1"
          exit
        }
        i = end
      }
      print "0"
    }
  ' "$file"
}

clock_exemption_is_allowlisted() {
  local file=$1 line=$2 source=$3
  # Exact file-and-line entries cap the exception inventory. The source check
  # prevents a permitted cooperative yield or process-local timeout from being
  # replaced by a different direct clock operation without review.
  case "$file:$line" in
    crates/lash-core/src/session/tool_execution.rs:571)
      [[ $source == *'tokio::task::yield_now()'* ]]
      ;; # Cooperative scheduling only; no time value participates in behavior.
    crates/lash-core/src/runtime/event_pump.rs:41)
      [[ $source == *'tokio::task::yield_now()'* ]]
      ;; # Cooperative scheduling only; no time value participates in behavior.
    crates/lash-core/src/runtime/commit_admission.rs:237)
      [[ $source == *'tokio::time::sleep(self.inner.wait_ttl)'* ]]
      ;; # Process-local admission timeout; no durable timestamp or ordering fact.
    *)
      return 1
      ;;
  esac
}

capture_search "clock discipline" "$clock_forbidden" "$tmp_dir/rule1.raw" \
  crates/lash-core/src crates/lash-core-ids/src crates/lash-core-llm/src
: >"$tmp_dir/rule1.hits"
while IFS=: read -r file line source; do
  [[ -n "$file" ]] || continue
  case "$file" in
    crates/lash-core/src/runtime/native_substrate/* | crates/lash-core-ids/src/clock.rs | \
    crates/lash-core-ids/src/test_clock.rs)
      continue
      ;;
  esac
  if [[ $file =~ $test_path_regex ]]; then
    continue
  fi
  if [[ $(line_in_test_region "$file" "$line") == 1 ]]; then
    continue
  fi
  if clock_exemption_is_allowlisted "$file" "$line" "$source"; then
    continue
  fi
  printf '%s:%s:%s\n' "$file" "$line" "$source" >>"$tmp_dir/rule1.hits"
done <"$tmp_dir/rule1.raw"
if [[ -s "$tmp_dir/rule1.hits" ]]; then
  cat "$tmp_dir/rule1.hits" >&2
  echo "substrate boundary rule 1 failed: direct Tokio clock use found in lash-core production source" >&2
  failed=1
fi

capture_search "module containment" "$containment_forbidden" "$tmp_dir/rule2.raw" \
  crates/lash-core/src crates/lash-core-ids/src crates/lash-core-llm/src crates/lash/src crates/lash-restate/src
: >"$tmp_dir/rule2.hits"
while IFS=: read -r file line source; do
  [[ -n "$file" ]] || continue
  if [[ $file =~ $containment_test_path_regex ]]; then
    continue
  fi
  if [[ $(line_in_test_region "$file" "$line") == 1 ]]; then
    continue
  fi
  case "$file" in
    crates/lash-core/src/runtime/native_substrate/* | \
      crates/lash-core/src/lib.rs | \
      crates/lash-core/src/runtime/mod.rs | \
      crates/lash-core/src/runtime/builder.rs | \
      crates/lash-core/src/runtime/environment.rs | \
      crates/lash-core/src/runtime/host.rs | \
      crates/lash-core/src/runtime/process_worker/mod.rs | \
      crates/lash-core/src/tool_provider.rs | \
      crates/lash-core/src/tool_provider/process_events.rs | \
      crates/lash/src/core.rs | \
      crates/lash/src/core/queued_work.rs | \
      crates/lash/src/core/work_drivers.rs | \
      crates/lash/src/lib.rs | \
      crates/lash/src/support.rs | \
      crates/lash/src/testing.rs)
      continue
      ;;
  esac
  printf '%s:%s:%s\n' "$file" "$line" "$source" >>"$tmp_dir/rule2.hits"
done <"$tmp_dir/rule2.raw"
if [[ -s "$tmp_dir/rule2.hits" ]]; then
  cat "$tmp_dir/rule2.hits" >&2
  echo "substrate boundary rule 2 failed: native substrate implementation vocabulary escaped its allowed modules" >&2
  failed=1
fi

capture_search "fallback shape" "$fallback_forbidden" "$tmp_dir/rule3.hits" \
  crates/lash-core/src crates/lash-core-ids/src crates/lash-core-llm/src crates/lash/src crates/lash-restate/src
if [[ -s "$tmp_dir/rule3.hits" ]]; then
  cat "$tmp_dir/rule3.hits" >&2
  echo "substrate boundary rule 3 failed: removed polling or optional-port fallback shape found" >&2
  failed=1
fi

capture_search "capability query" "$capability_forbidden" "$tmp_dir/rule4.hits" crates
if [[ -s "$tmp_dir/rule4.hits" ]]; then
  cat "$tmp_dir/rule4.hits" >&2
  echo "substrate boundary rule 4 failed: capability-query declaration, call, or field found" >&2
  failed=1
fi

# Rule 5 — drive determinism ratchet.
#
# The turn driver is workflow code: on replay it must re-issue exactly the
# commands the journal recorded, so drive code may not reach facilities whose
# results depend on scheduling, the wall clock, process-global state or live
# stores. The scanned modules are the drive path the FIG-3672 inventory walked:
#
#   crates/lash-core/src/runtime/{turn_loop,turn_driver}/**, logical_turn.rs,
#   turn_boundary*                       -- the loop around the driver
#   crates/lash-core-execution/src/session{,.rs}, tool_dispatch{,.rs},
#   runtime/effect/{tool_child_driver.rs,group*.rs}
#                                        -- execution-side session and group
#                                           child drive code
#   crates/lash-protocol-rlm/src/{executor,projection}/**
#                                        -- the code cell's host bridge
#   crates/lashlang/src/**               -- the VM crate (the plan's V/ prefix)
#   crates/lash-lashlang-runtime/src/**  -- the lashlang runtime
#
# The inventory's "R/ handler code outside ctx.run closures" is approximated by
# a path filter on the Restate handler modules -- controller/, effect_group{.rs,/},
# process/ and durable_wait.rs -- because a line lint cannot tell handler code
# from a ctx.run closure body. That over-catches legal recorded bodies; those
# sites are simply allowlisted like the rest.
#
# Forbidden constructs: Tokio scheduling and time (spawn/select!/join!/sync::/
# time::/task::/task_local!, including grouped `use tokio::{...}` imports),
# futures::join_all, Instant::now, SystemTime, SystemClock, Uuid::new_v4,
# rand::, block_on, `dyn Future + Send`, `.await` on a store trait (approximated
# as a store|registry-named receiver or callee with .await in the same
# expression -- ripgrep searches multiline, the grep fallback is single-line),
# and HashMap/HashSet mentions (a superset of the plan's "no iterating hash
# collections": FxHash maps use a fixed hasher and stay legal).
#
# Every current hit is pinned in scripts/drive-determinism-allowlist.txt as
# `path:line  # <inventory id>`; the ratchet test only lets that file shrink.

drive_paths=(
  crates/lash-core/src/runtime/turn_loop
  crates/lash-core/src/runtime/turn_driver
  crates/lash-core/src/runtime/logical_turn.rs
  crates/lash-core/src/runtime/turn_boundary*
  crates/lash-core-execution/src/session.rs
  crates/lash-core-execution/src/session
  crates/lash-core-execution/src/tool_dispatch.rs
  crates/lash-core-execution/src/tool_dispatch
  crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs
  crates/lash-core-execution/src/runtime/effect/group*.rs
  crates/lash-protocol-rlm/src/executor
  crates/lash-protocol-rlm/src/projection
  crates/lashlang/src
  crates/lash-lashlang-runtime/src
  crates/lash-restate/src/controller
  crates/lash-restate/src/effect_group
  crates/lash-restate/src/effect_group.rs
  crates/lash-restate/src/process
  crates/lash-restate/src/durable_wait.rs
)

drive_forbidden='tokio::(spawn|select|join|sync::|time::|task::|task_local!)|use[[:space:]]+tokio::\{[^}]*\b(spawn|select|join|sync|time|task)|futures::(future::)?join_all|(^|[^[:alnum:]_])(Instant::now|SystemTime|SystemClock|Uuid::new_v4|block_on)([^[:alnum:]_]|$)|(^|[^[:alnum:]_])rand::|dyn[[:space:]]+Future[^;]{0,160}\+[[:space:]]*Send|(^|[^[:alnum:]_])(HashMap|HashSet)([^[:alnum:]_]|$)'
# One match ends at its first `.await`, so the `-U` output's `.await` line is
# the one line pinned per call. The grep fallback only sees single-line calls.
drive_store_await='[a-zA-Z_]*(store|registry|Store|Registry)[a-zA-Z_]*[[:space:]]*(\.[[:space:]]*[a-z_]+[[:space:]]*)?\([^;]{0,800}?\.await([^[:alnum:]_]|$)'
drive_store_await_line='(store|registry|Store|Registry)[^;]{0,200}\.await([^[:alnum:]_]|$)'
drive_allowlist=scripts/drive-determinism-allowlist.txt

capture_search "drive determinism" "$drive_forbidden" "$tmp_dir/rule5.raw" "${drive_paths[@]}"
if command -v rg >/dev/null 2>&1; then
  if rg -n -U --glob '*.rs' "$drive_store_await" "${drive_paths[@]}" >"$tmp_dir/rule5.store.multi"; then
    grep -E '\.await([^[:alnum:]_]|$)' "$tmp_dir/rule5.store.multi" >"$tmp_dir/rule5.store" || true
  else
    store_status=$?
    if [[ $store_status -ne 1 ]]; then
      echo "substrate boundary check failed: drive determinism store-await search exited $store_status" >&2
      failed=1
    fi
    : >"$tmp_dir/rule5.store"
  fi
else
  if search_rust "$drive_store_await_line" "${drive_paths[@]}" >"$tmp_dir/rule5.store"; then
    :
  else
    store_status=$?
    if [[ $store_status -ne 1 ]]; then
      echo "substrate boundary check failed: drive determinism store-await search exited $store_status" >&2
      failed=1
    fi
    : >"$tmp_dir/rule5.store"
  fi
fi
cat "$tmp_dir/rule5.store" >>"$tmp_dir/rule5.raw"
sort -u -o "$tmp_dir/rule5.raw" "$tmp_dir/rule5.raw" 2>/dev/null || true

declare -A drive_allowed=()
if [[ -f $drive_allowlist ]]; then
  while read -r allowed_ref _; do
    [[ -n $allowed_ref && $allowed_ref != \#* ]] && drive_allowed[$allowed_ref]=1
  done <"$drive_allowlist"
else
  echo "substrate boundary check failed: $drive_allowlist is missing" >&2
  failed=1
fi

: >"$tmp_dir/rule5.hits"
: >"$tmp_dir/rule5.seen"
while IFS=: read -r file line source; do
  [[ -n "$file" ]] || continue
  if [[ $file =~ $test_path_regex ]]; then
    continue
  fi
  if [[ $(line_in_test_region "$file" "$line") == 1 ]]; then
    continue
  fi
  printf '%s:%s\n' "$file" "$line" >>"$tmp_dir/rule5.seen"
  if [[ -z ${drive_allowed[$file:$line]+x} ]]; then
    printf '%s:%s:%s\n' "$file" "$line" "$source" >>"$tmp_dir/rule5.hits"
  fi
done <"$tmp_dir/rule5.raw"
if [[ -s "$tmp_dir/rule5.hits" ]]; then
  cat "$tmp_dir/rule5.hits" >&2
  echo "substrate boundary rule 5 failed: nondeterministic facility found in drive code" >&2
  echo "  (new sites belong behind a recorded step; if one is deliberate, pin it in $drive_allowlist with its inventory id)" >&2
  failed=1
fi
if [[ -f $drive_allowlist ]]; then
  while read -r allowed_ref _; do
    [[ -n $allowed_ref && $allowed_ref != \#* ]] || continue
    if ! grep -qxF "$allowed_ref" "$tmp_dir/rule5.seen"; then
      echo "substrate boundary rule 5 failed: stale allowlist entry $allowed_ref (site moved or fixed; regenerate the allowlist)" >&2
      failed=1
    fi
  done <"$drive_allowlist"
fi

if [[ $failed -ne 0 ]]; then
  exit 1
fi
