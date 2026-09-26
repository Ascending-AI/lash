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
capability_names='replay_ownership|journal_addressing|durable_workflow_controller|allows_process_lifetime_completion_keys|turn_control_participation|runtime_effect_failure_disposition|effect_journaling|turn_control_authority_owner'
capability_forbidden="fn[[:space:]]+(${capability_names})([^[:alnum:]_]|$)|\.(${capability_names})[[:space:]]*\(|(^|[^[:alnum:]_])(${capability_names})[[:space:]]*:"
# Every effect host journals (FIG-3585): the in-process native tier, its
# journaling fact, store-delegated turn control and in-memory persistence are
# deleted, and none of their types may come back under any shape.
retired_type_names='EffectJournaling|TurnControlAuthorityOwner|NativeEffectHost|NativeRuntimeEffectController|NativeEffectGroups|NativeAwaitEventAuthority|AwaitEventRegistry|StoreTurnCancellationAuthority|InMemorySessionStore|InMemorySessionStoreFactory|TestLocalProcessRegistry'
retired_type_forbidden="(^|[^[:alnum:]_])(${retired_type_names})([^[:alnum:]_]|$)"
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

# Rule 4 scans every Rust tree that links lash: the crates, and the examples
# and runbooks where a tree has them.
rule4_roots=(crates)
for root in examples runbooks; do
  [[ -d $root ]] && rule4_roots+=("$root")
done

capture_search "capability query" "$capability_forbidden" "$tmp_dir/rule4.hits" "${rule4_roots[@]}"
if [[ -s "$tmp_dir/rule4.hits" ]]; then
  cat "$tmp_dir/rule4.hits" >&2
  echo "substrate boundary rule 4 failed: capability-query declaration, call, or field found" >&2
  failed=1
fi

capture_search "retired native tier" "$retired_type_forbidden" "$tmp_dir/rule4b.hits" "${rule4_roots[@]}"
if [[ -s "$tmp_dir/rule4b.hits" ]]; then
  cat "$tmp_dir/rule4b.hits" >&2
  echo "substrate boundary rule 4 failed: a retired native-tier or in-memory type name was found" >&2
  failed=1
fi

# The kernel names no engine (ADR 0104 §2): the execution identifiers that
# named Restate moved to engine-neutral names under FIG-3670. Only the engine
# crate, its test crate, and deployments that are explicitly Restate may still
# spell the retired identifiers.
engine_id_forbidden='(^|[^[:alnum:]_])(restate_invocation_id|restate_process_execution)([^[:alnum:]_]|$)'
capture_search "engine execution identifiers" "$engine_id_forbidden" "$tmp_dir/rule4c.raw" "${rule4_roots[@]}"
: >"$tmp_dir/rule4c.hits"
while IFS=: read -r file line source; do
  [[ -n "$file" ]] || continue
  case "$file" in
    crates/lash-restate/* | crates/lash-restate-test/* | \
      examples/agent-service/* | examples/agent-workbench/* | \
      runbooks/restate-postgres-workers/*)
      continue
      ;;
  esac
  printf '%s:%s:%s\n' "$file" "$line" "$source" >>"$tmp_dir/rule4c.hits"
done <"$tmp_dir/rule4c.raw"
if [[ -s "$tmp_dir/rule4c.hits" ]]; then
  cat "$tmp_dir/rule4c.hits" >&2
  echo "substrate boundary rule 4 failed: an engine-named execution identifier was found outside the engine crates" >&2
  failed=1
fi

# The error vocabulary is engine-neutral too (FIG-3670 error-code slice):
# `RuntimeErrorCode` variants and their wiring may not carry a `Restate*`
# name. The check is scoped to the runtime_error* files so engine-owned
# Restate types elsewhere stay legal.
engine_error_roots=()
for path in crates/lash-core-store/src/runtime_error.rs \
  crates/lash-core-store/src/runtime_error_tests.rs \
  crates/lash-core-store/src/runtime_error; do
  [[ -e $path ]] && engine_error_roots+=("$path")
done
if [[ ${#engine_error_roots[@]} -gt 0 ]]; then
  engine_error_forbidden='(^|[^[:alnum:]_])Restate[A-Z][A-Za-z]*'
  capture_search "engine-named error codes" "$engine_error_forbidden" "$tmp_dir/rule4d.hits" "${engine_error_roots[@]}"
  if [[ -s "$tmp_dir/rule4d.hits" ]]; then
    cat "$tmp_dir/rule4d.hits" >&2
    echo "substrate boundary rule 4 failed: a Restate-named RuntimeErrorCode variant was found" >&2
    failed=1
  fi
fi

# The facade's durable-format table names no engine either (FIG-3670 format
# slice, ADR 0104 §2): the formats an engine writes are rows the engine
# registers under `lash::restate`, so the table and the preflight that walks
# it may not spell `Restate*` identifiers. The check is scoped to the format
# table and preflight files so `lash::restate` and the engine crate keep
# naming their own formats.
engine_format_roots=()
for path in crates/lash/src/formats.rs crates/lash/src/preflight.rs \
  crates/lash/src/preflight; do
  [[ -e $path ]] && engine_format_roots+=("$path")
done
if [[ ${#engine_format_roots[@]} -gt 0 ]]; then
  engine_format_forbidden='(^|[^[:alnum:]_])Restate[A-Z][A-Za-z]*'
  capture_search "engine-named durable formats" "$engine_format_forbidden" "$tmp_dir/rule4e.hits" "${engine_format_roots[@]}"
  if [[ -s "$tmp_dir/rule4e.hits" ]]; then
    cat "$tmp_dir/rule4e.hits" >&2
    echo "substrate boundary rule 4 failed: a Restate-named durable-format identifier was found in the facade's format table or preflight" >&2
    failed=1
  fi
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
#   crates/lash-core/src/runtime/drive{.rs,/**}
#                                        -- the session drive and its admission
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
# rand::, block_on, `dyn Future + Send`, and HashMap/HashSet mentions (a superset of the plan's "no iterating hash
# collections": FxHash maps use a fixed hasher and stay legal).
#
# Every current hit is pinned in scripts/drive-determinism-allowlist.txt as
# `path  |  <normalized line text>  |  <occurrence count>  # <inventory id>`,
# where the text is the offending line trimmed with internal whitespace
# collapsed. Entries key on the matched text, not the line number, so an
# unrelated edit that shifts lines in a drive file does not break the check;
# a hit fails when its (file, text) is unlisted or occurs more times than
# pinned, and a pinned entry that occurs fewer times than listed is stale and
# fails, so later slices must delete or decrement their lines. The ratchet
# test only lets the total occurrence count shrink.

drive_paths=(
  crates/lash-core/src/runtime/drive.rs
  crates/lash-core/src/runtime/drive
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
drive_allowlist=scripts/drive-determinism-allowlist.txt

# Always grep -E, never ripgrep: the two engines disagree on these patterns,
# and CI runners do not all carry ripgrep, so one engine keeps the allowlist
# identical everywhere.
drive_search_status=0
# Scan only the drive paths that exist: a tree may predate a newer drive
# module, and a missing path must not read as a failed search.
drive_existing=()
for drive_path in "${drive_paths[@]}"; do
  [[ -e $drive_path ]] && drive_existing+=("$drive_path")
done
if ((${#drive_existing[@]})); then
  grep -rEn --include='*.rs' "$drive_forbidden" "${drive_existing[@]}" >"$tmp_dir/rule5.raw" 2>/dev/null || drive_search_status=$?
else
  : >"$tmp_dir/rule5.raw"
fi
if [[ $drive_search_status -gt 1 ]]; then
  echo "substrate boundary check failed: drive determinism search exited $drive_search_status" >&2
  failed=1
fi
sort -u -o "$tmp_dir/rule5.raw" "$tmp_dir/rule5.raw" 2>/dev/null || true

# A hit's key is (file, normalized line text): trimmed, with every internal
# whitespace run collapsed to one space, so edits that move or reindent the
# line keep it pinned. The allowlist's `  |  ` and `  # ` separators use a
# double space, which normalized text can never contain.
drive_normalize() {
  local text=$1
  text="$(printf '%s' "$text" | tr -s '[:space:]' ' ')"
  text="${text# }"
  text="${text% }"
  printf '%s' "$text"
}

declare -A drive_allowed=()
if [[ -f $drive_allowlist ]]; then
  while IFS= read -r entry || [[ -n $entry ]]; do
    [[ $entry == \#* || -z ${entry//[[:space:]]/} ]] && continue
    body=${entry%%  # *}
    allowed_file=${body%%  |  *}
    allowed_rest=${body#*  |  }
    allowed_text=${allowed_rest%%  |  *}
    allowed_count=${allowed_rest##*  |  }
    if [[ $body != *'  |  '* || $allowed_rest != *'  |  '* || -z $allowed_file \
      || -z $allowed_text || ! $allowed_count =~ ^[0-9]+$ ]]; then
      echo "substrate boundary check failed: malformed allowlist entry: $entry" >&2
      failed=1
      continue
    fi
    drive_allowed["$allowed_file|$allowed_text"]=$allowed_count
  done <"$drive_allowlist"
else
  echo "substrate boundary check failed: $drive_allowlist is missing" >&2
  failed=1
fi

declare -A drive_seen=()
: >"$tmp_dir/rule5.hits"
while IFS=: read -r file line source; do
  [[ -n "$file" ]] || continue
  if [[ $file =~ $test_path_regex ]]; then
    continue
  fi
  if [[ $(line_in_test_region "$file" "$line") == 1 ]]; then
    continue
  fi
  printf '%s:%s:%s\n' "$file" "$line" "$source" >>"$tmp_dir/rule5.hits"
  key="$file|$(drive_normalize "$source")"
  drive_seen[$key]=$(( ${drive_seen[$key]:-0} + 1 ))
done <"$tmp_dir/rule5.raw"

if [[ ${DRIVE_DETERMINISM_REGENERATE:-0} == 1 ]]; then
  # Rewrite the allowlist from the current tree, keeping each surviving
  # entry's inventory tag; new keys are tagged UNMAPPED.
  declare -A drive_tags=()
  while IFS= read -r entry || [[ -n $entry ]]; do
    [[ $entry == \#* || -z ${entry//[[:space:]]/} || $entry != *'  # '* ]] && continue
    body=${entry%%  # *}
    tag=${entry#*  # }
    tfile=${body%%  |  *}; trest=${body#*  |  }; ttext=${trest%%  |  *}
    drive_tags["$tfile|$ttext"]=$tag
  done < <(grep -v '^#' "$drive_allowlist" 2>/dev/null || true)
  header=$(grep '^#' "$drive_allowlist" 2>/dev/null || true)
  total=0
  {
    [[ -n $header ]] && printf '%s\n' "$header"
    for key in "${!drive_seen[@]}"; do
      printf '%s  |  %s  |  %s  # %s\n' "${key%%|*}" "${key#*|}" "${drive_seen[$key]}" "${drive_tags[$key]:-UNMAPPED}"
    done | LC_ALL=C sort
  } >"$drive_allowlist.new"
  for key in "${!drive_seen[@]}"; do total=$(( total + drive_seen[$key] )); done
  mv "$drive_allowlist.new" "$drive_allowlist"
  printf '%s\n' "$total" >scripts/drive-determinism-allowlist.count
  echo "regenerated $drive_allowlist: ${#drive_seen[@]} entries, $total occurrences"
  exit 0
fi

declare -A drive_bad=()
if [[ ${#drive_seen[@]} -gt 0 ]]; then
  for key in "${!drive_seen[@]}"; do
    if [[ ${drive_seen[$key]} -gt ${drive_allowed[$key]:-0} ]]; then
      drive_bad[$key]=1
    fi
  done
fi
if [[ ${#drive_bad[@]} -gt 0 ]]; then
  while IFS=: read -r file line source; do
    [[ -n "$file" ]] || continue
    key="$file|$(drive_normalize "$source")"
    if [[ -n ${drive_bad[$key]+x} ]]; then
      printf '%s:%s:%s\n' "$file" "$line" "$source" >&2
    fi
  done <"$tmp_dir/rule5.hits"
  echo "substrate boundary rule 5 failed: nondeterministic facility found in drive code" >&2
  echo "  (new sites belong behind a recorded step; if one is deliberate, pin it in $drive_allowlist with its inventory id)" >&2
  failed=1
fi
if [[ ${#drive_allowed[@]} -gt 0 ]]; then
  for key in "${!drive_allowed[@]}"; do
    actual=${drive_seen[$key]:-0}
    if [[ $actual -lt ${drive_allowed[$key]} ]]; then
      stale_file=${key%%|*}
      stale_text=${key#*|}
      echo "substrate boundary rule 5 failed: stale allowlist entry $stale_file  |  $stale_text  |  ${drive_allowed[$key]} (occurs $actual time(s)); remove or decrement it" >&2
      failed=1
    fi
  done
fi

if [[ $failed -ne 0 ]]; then
  exit 1
fi
