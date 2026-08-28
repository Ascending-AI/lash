#!/usr/bin/env bash
set -euo pipefail

# Clock-rule scope exemptions outside lash-core (inventory re-derived at 2fe260e7e):
# - No lash-core dependency: lash-llm-transport, lash-http-transport, lash-plugin-mcp,
#   lash-tools.
# - Clock implementation: lash-sim/src/clock.rs:64.
# - Benchmark harness (24 sites):
#   lash-perf/src/runtime_perf/measurement/store_hardening.rs:229;
#   lash-perf/src/runtime_perf/measurement/provider_scenarios.rs:62,213;
#   lash-perf/src/runtime_perf/harness/observation.rs:98;
#   lash-perf/src/runtime_perf/measurement/checkpoint_curve.rs:360;
#   lash-perf/src/runtime_perf/measurement/queued_work.rs:263,773;
#   lash-perf/src/runtime_perf/providers.rs:677,692,696,730,969,1061,1120;
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

clock_forbidden='tokio::time::(sleep|sleep_until|interval)|tokio::task::yield_now'
containment_forbidden='(^|[^[:alnum:]_])(NativeQueuedWork|NoQueuedWork|NativeProcessWork|NativeProcessAwaiter|NativeQueuedWorkRunHandle|NativeSubstrateSetup|NativeSubstrateSlot|WakeDeliveryDriver)([^[:alnum:]_]|$)'
fallback_forbidden='ProcessAwaiter::polling|Option[[:space:]]*<[[:space:]]*Arc[[:space:]]*<[[:space:]]*dyn[[:space:]]+(QueuedWorkSubstrate|ProcessWorkSubstrate)[[:space:]]*>[[:space:]]*>|Option[[:space:]]*<[[:space:]]*(ProcessWorkDriver|QueuedWorkDriver)[[:space:]]*>'
capability_names='replay_ownership|journal_addressing|durable_workflow_controller|allows_process_lifetime_completion_keys'
capability_forbidden="fn[[:space:]]+(${capability_names})([^[:alnum:]_]|$)|\.(${capability_names})[[:space:]]*\(|(^|[^[:alnum:]_])(${capability_names})[[:space:]]*:"
test_path_regex='(^|/)(tests?|testing|[a-z_]*_tests)(/|\.rs$)'
containment_test_path_regex='(^|/)(tests?|testing|[a-z_]*_tests)(/|\.rs$)|_tests\.rs$'

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

test_region_start_line() {
  local file=$1
  # A production file can contain an earlier closed cfg(test) module and then
  # resume production code. The final cfg(test) module is the bottom test
  # region covered by A13's post-filter contract.
  grep -nE '^[[:space:]]*#\[cfg\(test\)\]' "$file" | tail -1 | cut -d: -f1 || true
}

capture_search "clock discipline" "$clock_forbidden" "$tmp_dir/rule1.raw" crates/lash-core/src
: >"$tmp_dir/rule1.hits"
while IFS=: read -r file line source; do
  [[ -n "$file" ]] || continue
  case "$file" in
    crates/lash-core/src/runtime/native_substrate/* | crates/lash-core/src/runtime/clock.rs)
      continue
      ;;
  esac
  if [[ $file =~ $test_path_regex ]]; then
    continue
  fi
  cfg_test_line=$(test_region_start_line "$file")
  if [[ -n $cfg_test_line && $line -ge $cfg_test_line ]]; then
    continue
  fi
  preceding_line=''
  if [[ $line -gt 1 ]]; then
    preceding_line=$(sed -n "$((line - 1))p" "$file")
  fi
  if [[ $preceding_line == *'clock-exempt:'* ]]; then
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
  crates/lash-core/src crates/lash/src
: >"$tmp_dir/rule2.hits"
while IFS=: read -r file line source; do
  [[ -n "$file" ]] || continue
  if [[ $file =~ $containment_test_path_regex ]]; then
    continue
  fi
  cfg_test_line=$(test_region_start_line "$file")
  if [[ -n $cfg_test_line && $line -ge $cfg_test_line ]]; then
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
  crates/lash-core/src crates/lash/src
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

if [[ $failed -ne 0 ]]; then
  exit 1
fi
