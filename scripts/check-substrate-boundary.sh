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
# - Store-local retry: lash-postgres-store/src/postgres/attachments.rs:108.
# - Other deliberately out-of-scope sites: lash/src/session.rs:983;
#   lash-provider-openai/src/codex/ws_testing.rs:286,468;
#   lash-protocol-rlm/src/executor/host_bridge.rs:1040;
#   lash-sim/src/backend_contention.rs:536;
#   lash-sim/src/runner/generated_world.rs:551,903.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

clock_forbidden='tokio::time::(sleep|sleep_until|interval)|tokio::task::yield_now|use[[:space:]]+tokio::time::\{[^}]*(sleep|sleep_until|interval)'
containment_forbidden='(^|[^[:alnum:]_])(NoQueuedWork|WakeDeliveryDriver)([^[:alnum:]_]|$)'
fallback_forbidden='ProcessAwaiter::polling|Option[[:space:]]*<[[:space:]]*Arc[[:space:]]*<[[:space:]]*dyn[[:space:]]+(QueuedWorkSubstrate|ProcessWorkSubstrate)[[:space:]]*>[[:space:]]*>|Option[[:space:]]*<[[:space:]]*(ProcessWorkDriver|QueuedWorkDriver)[[:space:]]*>'
capability_names='replay_ownership|journal_addressing|durable_workflow_controller|allows_process_lifetime_completion_keys|turn_control_participation|runtime_effect_failure_disposition|effect_journaling|turn_control_authority_owner'
capability_forbidden="fn[[:space:]]+(${capability_names})([^[:alnum:]_]|$)|\.(${capability_names})[[:space:]]*\(|(^|[^[:alnum:]_])(${capability_names})[[:space:]]*:"
# Historical, retired in 60e0e86b2a: every effect host journals (FIG-3585): the in-process native tier, its
# journaling fact, store-delegated turn control and in-memory persistence are
# deleted, and none of their types may come back under any shape. Restate is
# the only engine (ADR 0104): the in-process session and process work, its
# substrate config and the worker's self-driven sweep are deleted too.
retired_type_names='EffectJournaling|TurnControlAuthorityOwner|NativeEffectHost|NativeRuntimeEffectController|NativeEffectGroups|NativeAwaitEventAuthority|AwaitEventRegistry|StoreTurnCancellationAuthority|InMemorySessionStore|InMemorySessionStoreFactory|TestLocalProcessRegistry|NativeQueuedWork|NativeQueuedWorkRunHandle|InlineSessionWork|NativeProcessWork|NativeProcessAwaiter|NativeSubstrateSetup|NativeSubstrateSlot|NativeSubstrateConfig|WorkerProcessWork|WorkerSweepPolicy'
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
    crates/lash-core/src/runtime/commit_admission.rs:229)
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
  crates/lash-core/src crates/lash-core-ids/src crates/lash-core-llm/src crates/lash/src
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
    crates/lash-core/src/lib.rs | \
      crates/lash-core/src/runtime/mod.rs | \
      crates/lash-core/src/runtime/builder.rs | \
      crates/lash-core/src/runtime/environment.rs | \
      crates/lash-core/src/runtime/host.rs | \
      crates/lash-core/src/tool_provider.rs | \
      crates/lash-core/src/tool_provider/process_events.rs | \
      crates/lash/src/core.rs | \
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
  crates/lash-core/src crates/lash-core-ids/src crates/lash-core-llm/src crates/lash/src
if [[ -s "$tmp_dir/rule3.hits" ]]; then
  cat "$tmp_dir/rule3.hits" >&2
  echo "substrate boundary rule 3 failed: removed polling or optional-port fallback shape found" >&2
  failed=1
fi

# Rule 4 scans every Rust tree that links lash: the crates, and the examples
# and runbooks where a tree has them.
rule4_runs=(crates)
for root in examples runbooks; do
  [[ -d $root ]] && rule4_runs+=("$root")
done

capture_search "capability query" "$capability_forbidden" "$tmp_dir/rule4.hits" "${rule4_runs[@]}"
if [[ -s "$tmp_dir/rule4.hits" ]]; then
  cat "$tmp_dir/rule4.hits" >&2
  echo "substrate boundary rule 4 failed: capability-query declaration, call, or field found" >&2
  failed=1
fi

capture_search "retired native tier" "$retired_type_forbidden" "$tmp_dir/rule4b.hits" "${rule4_runs[@]}"
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
capture_search "engine execution identifiers" "$engine_id_forbidden" "$tmp_dir/rule4c.raw" "${rule4_runs[@]}"
: >"$tmp_dir/rule4c.hits"
while IFS=: read -r file line source; do
  [[ -n "$file" ]] || continue
  case "$file" in
    examples/agent-workbench/*)
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
engine_error_runs=()
for path in crates/lash-core-store/src/runtime_error.rs \
  crates/lash-core-store/src/runtime_error_tests.rs \
  crates/lash-core-store/src/runtime_error; do
  [[ -e $path ]] && engine_error_runs+=("$path")
done
if [[ ${#engine_error_runs[@]} -gt 0 ]]; then
  engine_error_forbidden='(^|[^[:alnum:]_])Restate[A-Z][A-Za-z]*'
  capture_search "engine-named error codes" "$engine_error_forbidden" "$tmp_dir/rule4d.hits" "${engine_error_runs[@]}"
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
engine_format_runs=()
for path in crates/lash/src/formats.rs crates/lash/src/preflight.rs \
  crates/lash/src/preflight; do
  [[ -e $path ]] && engine_format_runs+=("$path")
done
if [[ ${#engine_format_runs[@]} -gt 0 ]]; then
  engine_format_forbidden='(^|[^[:alnum:]_])Restate[A-Z][A-Za-z]*'
  capture_search "engine-named durable formats" "$engine_format_forbidden" "$tmp_dir/rule4e.hits" "${engine_format_runs[@]}"
  if [[ -s "$tmp_dir/rule4e.hits" ]]; then
    cat "$tmp_dir/rule4e.hits" >&2
    echo "substrate boundary rule 4 failed: a Restate-named durable-format identifier was found in the facade's format table or preflight" >&2
    failed=1
  fi
fi

# A pinned lash service (FIG-3795: the process workflow and the session
# object's shift and turn) is addressed only through a
# `ServiceRoute`: the name a call targets is always a route — stable, or the
# generation lane a recorded route names — never the name a generated typed
# client bakes into the request target. Shared services keep their typed
# clients until FIG-3803 epoch-names them.
pinned_client_forbidden='(workflow_client|object_client|service_client)::[[:space:]]*<[[:space:]]*(LashProcessWorkflowClient|LashSessionClient|LashTurnClient)'
capture_search "pinned-service typed client" "$pinned_client_forbidden" "$tmp_dir/rule4f.raw" "${rule4_runs[@]}"
: >"$tmp_dir/rule4f.hits"
while IFS=: read -r file line source; do
  [[ -n "$file" ]] || continue
  if [[ $file =~ $test_path_regex ]]; then
    continue
  fi
  if [[ $(line_in_test_region "$file" "$line") == 1 ]]; then
    continue
  fi
  printf '%s:%s:%s\n' "$file" "$line" "$source" >>"$tmp_dir/rule4f.hits"
done <"$tmp_dir/rule4f.raw"
if [[ -s "$tmp_dir/rule4f.hits" ]]; then
  cat "$tmp_dir/rule4f.hits" >&2
  echo "substrate boundary rule 4 failed: a pinned lash service is called through a typed client; route it through services::routed_workflow with a ServiceRoute" >&2
  failed=1
fi

# Rule 7 — the effect seam is collapsed (ADR 0132; I0, FIG-5194). Every effect
# runs on the concrete `ActorContext`: the effect-host, controller and layer
# traits are deleted and none of their names may come back.
#
# `PluginError::RuntimeEffectController` (which carries a
# `RuntimeEffectControllerError`) and the wire kind
# `TurnFailureKind::RuntimeEffectController` are variants, not the trait:
# their paths, tuple patterns and the wire kind's declaration are not hits.
collapse_names='EffectEngine|EffectHost|RuntimeEffectController|ScopedEffectController|EffectTaskController|LayeredEngine|EffectLayer|LayeredEffectHost|AwaitEventResolver'
collapse_forbidden="(^|[^[:alnum:]_])(${collapse_names})([^[:alnum:]_]|$)"
capture_search "deleted effect seam" "$collapse_forbidden" "$tmp_dir/rule7.raw" "${rule4_runs[@]}"
: >"$tmp_dir/rule7.hits"
while IFS=: read -r file line source; do
  stripped="$(sed -E \
    -e 's/(PluginError|TurnFailureKind|Self)::RuntimeEffectController([^[:alnum:]_]|$)/\2/g' \
    -e 's/(^|[^[:alnum:]_:])RuntimeEffectController[[:space:]]*\(/\1(/g' <<<"$source")"
  if [[ $file == crates/lash-sansio/src/session_model/failure.rs ]]; then
    stripped="$(sed -E 's/^[[:space:]]*RuntimeEffectController,[[:space:]]*$//' <<<"$stripped")"
  fi
  grep -Eq "$collapse_forbidden" <<<"$stripped" || continue
  printf '%s:%s:%s\n' "$file" "$line" "$source" >>"$tmp_dir/rule7.hits"
done <"$tmp_dir/rule7.raw"
if [[ -s "$tmp_dir/rule7.hits" ]]; then
  cat "$tmp_dir/rule7.hits" >&2
  echo "substrate boundary rule 7 failed: a deleted effect-seam name was found; effects run on ActorContext" >&2
  failed=1
fi

# Rule 7c — generation lanes are gone (ADR 0132; L10g, FIG-5200). A node is
# not addressed by a build generation: the generation, its engine slot, the
# journal-logic epoch, the drain marks and their store, the generation fence,
# finalize and the deployment registry are deleted, and none of their names
# may come back. The names are matched as substrings, so a store table or
# helper that embeds one (`lash_draining_generations`) is a hit too.
generation_names='BuildGeneration|EngineGeneration|JOURNAL_LOGIC_EPOCH|generation_drain|fleet_finalize|DeploymentRegistry|draining_generations|generation_fence'
capture_search "deleted generation machinery" "$generation_names" "$tmp_dir/rule7c.hits" "${rule4_runs[@]}"
if [[ -s "$tmp_dir/rule7c.hits" ]]; then
  cat "$tmp_dir/rule7c.hits" >&2
  echo "substrate boundary rule 7c failed: a deleted generation-lane name was found; generation drain is gone (ADR 0132)" >&2
  failed=1
fi

# Rule 7b — no silent defaults (law S1). A method of `ProcessEngine`,
# `ProjectionProvider` or `DurableStore` has no default body: each
# implementation answers every method itself.
default_traits='ProcessEngine|ProjectionProvider|DurableStore'
capture_search "silent-default traits" "(^|[^[:alnum:]_])trait[[:space:]]+(${default_traits})([^[:alnum:]_]|$)" \
  "$tmp_dir/rule7b.raw" "${rule4_runs[@]}"
: >"$tmp_dir/rule7b.hits"
cut -d: -f1 "$tmp_dir/rule7b.raw" | sort -u | while IFS= read -r file; do
  # Print file:line:trait for each method inside one of the traits whose
  # signature ends in a body rather than `;`.
  awk -v traits="$default_traits" '
    FNR == 1 { depth = -1; sig = "" }
    {
      code = $0
      sub(/\/\/.*/, "", code)
      if (depth < 0) {
        if (!match(code, "(^|[^[:alnum:]_])trait[[:space:]]+(" traits ")([^[:alnum:]_]|$)")) {
          next
        }
        trait = code
        sub(/.*trait[[:space:]]+/, "", trait)
        sub(/[^[:alnum:]_].*/, "", trait)
        depth = 0
        opened = 0
      }
      if (sig != "" || (depth == 1 && code ~ /^[[:space:]]*(pub[[:space:]]+)?(async[[:space:]]+)?(unsafe[[:space:]]+)?fn[[:space:]]/)) {
        if (sig == "") {
          sig_line = FNR
        }
        sig = sig code
        if (code ~ /;[[:space:]]*$/) {
          sig = ""
        } else if (code ~ /\{/) {
          print FILENAME ":" sig_line ":" trait " has a default method body"
          sig = ""
        }
      }
      opens = gsub(/\{/, "{", code)
      closes = gsub(/\}/, "}", code)
      depth += opens - closes
      if (opens > 0) {
        opened = 1
      }
      if (opened && depth <= 0) {
        depth = -1
      }
    }
  ' "$file"
done >"$tmp_dir/rule7b.hits"
if [[ -s "$tmp_dir/rule7b.hits" ]]; then
  cat "$tmp_dir/rule7b.hits" >&2
  echo "substrate boundary rule 7 failed: a method of ProcessEngine, ProjectionProvider or DurableStore has a default body" >&2
  failed=1
fi

# Rule 7d — no shift fence on the durable path (ADR 0132; L3s, FIG-5196).
# Session work arrives as mail and the session actor's claim is the
# admission: the shift, its epoch fence and the turn parks are deleted, and
# none of their names may come back. A comment line is history, not a path,
# so it is not a hit.
shift_names='ShiftFence|ShiftEpoch|ShiftEpochSeal|ShiftEpochStore|seal_shift_epoch|seal_shift_epoch_for_test|supersede_shift_epoch_for_test|RunStartNonce|RunHold|ShiftHold|ShiftLoop|ShiftRequest|RuntimeStoreTestShiftExt|shift_fence|shift_epoch|shift_admission_id|shift_run_start|lash_session_shift_admissions|session_shift_admissions|ck_session_meta_shift_authority|turn_parks|lash_turn_parks|turn_park_clock|lash_turn_park_clock|turn_park_events|lash_turn_park_events|TurnParkWrite|TurnParkOrigin|record_turn_park|load_turn_park|turn_park_feed'
shift_forbidden="(^|[^[:alnum:]_])(${shift_names})([^[:alnum:]_]|$)"
capture_search "deleted shift fence" "$shift_forbidden" "$tmp_dir/rule7c.raw" "${rule4_runs[@]}"
: >"$tmp_dir/rule7c.hits"
while IFS=: read -r file line source; do
  [[ -n "$file" ]] || continue
  [[ $source =~ ^[[:space:]]*// ]] && continue
  printf '%s:%s:%s\n' "$file" "$line" "$source" >>"$tmp_dir/rule7c.hits"
done <"$tmp_dir/rule7c.raw"
if [[ -s "$tmp_dir/rule7c.hits" ]]; then
  cat "$tmp_dir/rule7c.hits" >&2
  echo "substrate boundary rule 7d failed: a deleted shift-fence or turn-park name was found; session work arrives as mail" >&2
  failed=1
fi

if [[ $failed -ne 0 ]]; then
  exit 1
fi
