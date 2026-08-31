#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"

export PATH="$HOME/.cargo/bin:$PATH"

dry_run=0
requested_selector=""
for argument in "$@"; do
  case "$argument" in
    --dry-run) dry_run=1 ;;
    -h|--help) requested_selector="$argument" ;;
    *)
      if [ -n "$requested_selector" ]; then
        echo "Expected one confidence selector, got: $*" >&2
        exit 2
      fi
      requested_selector="$argument"
      ;;
  esac
done
requested_selector="${requested_selector:-default}"
requested_lane="${requested_selector%%+*}"
area="all"
if [[ "$requested_selector" == *+* ]]; then
  area_selector="${requested_selector#*+}"
  if [[ "$area_selector" != area:* ]] || [[ "$area_selector" == *+* ]]; then
    echo "Invalid confidence selector '${requested_selector}'." >&2
    echo "Expected <lane>[+area:<surface>]; run with --help for the full vocabulary." >&2
    exit 2
  fi
  area="${area_selector#area:}"
fi

lane="$requested_lane"
fast_shard="all"
sim_search_shard=""
if [[ "$requested_lane" == fast:* ]]; then
  lane="fast"
  fast_shard="${requested_lane#fast:}"
fi
if [[ "$requested_lane" == sim-search:* ]]; then
  lane="full"
  sim_search_shard="${requested_lane#sim-search:}"
fi

areas=(store process trigger effect-host protocol provider sim)
area_is_known=0
for known_area in "${areas[@]}"; do
  if [ "$area" = "$known_area" ]; then
    area_is_known=1
    break
  fi
done
if [ "$area" != "all" ] && [ "$area_is_known" -ne 1 ]; then
  echo "Unknown confidence area '${area}'." >&2
  echo "Areas: store, process, trigger, effect-host, protocol, provider, sim" >&2
  exit 2
fi
if [ -n "$sim_search_shard" ] && [ "$area" != "all" ] && [ "$area" != "sim" ]; then
  echo "sim-search shards may only compose with area:sim, got area:${area}." >&2
  exit 2
fi

out_root="${LASH_CONFIDENCE_OUT_DIR:-$repo/target/confidence/$LASH_GATE_WORKTREE_SLUG}"
if [ -n "$sim_search_shard" ]; then
  out_dir="${out_root}/sim-search/${sim_search_shard//\//-of-}"
elif [ "$lane" = "fast" ] && [ "$fast_shard" != "all" ]; then
  if [ "$fast_shard" = "summary" ]; then
    out_dir="${out_root}/fast"
  else
    out_dir="${out_root}/fast/${fast_shard}"
  fi
else
  out_dir="${out_root}/${lane}"
fi
if [ "$area" != "all" ]; then
  out_dir="${out_dir}/areas/${area}"
fi
ci_features="${LASH_CI_FEATURES:-}"
critical_packages=(
  lash-internal-core
  lash-internal-lashlang
  lash-internal-protocol-rlm
  lash-internal-protocol-standard
  lash-internal-sqlite-store
  lash-internal-postgres-store
)
selected_packages=()
area_mutation_file_args=()
if [ "$area" = "all" ]; then
  selected_packages=("${critical_packages[@]}")
else
  case "$area" in
    store) selected_packages=(lash-internal-sqlite-store lash-internal-postgres-store) ;;
    process|trigger|effect-host|provider) selected_packages=(lash-internal-core) ;;
    protocol) selected_packages=(lash-internal-lashlang lash-internal-protocol-rlm lash-internal-protocol-standard) ;;
    sim) selected_packages=(lash-sim) ;;
  esac
  case "$area" in
    process)
      area_mutation_file_args=(
        --file 'crates/lash-core/src/runtime/process.rs'
        --file 'crates/lash-core/src/runtime/process/*.rs'
        --file 'crates/lash-core/src/runtime/process_worker/*.rs'
        --file 'crates/lash-core/src/runtime/process_work_driver.rs'
        --file 'crates/lash-core/src/runtime/queued_work_driver.rs'
        --file 'crates/lash-core/src/runtime/wake_delivery_driver.rs'
        --file 'crates/lash-core/src/session/process_handles.rs'
        --file 'crates/lash-core/src/tool_provider/process*.rs'
      )
      ;;
    trigger)
      area_mutation_file_args=(
        --file 'crates/lash-core/src/triggers.rs'
        --file 'crates/lash-core/src/triggers/*.rs'
        --file 'crates/lash-core/src/plugin/trigger_registry.rs'
        --file 'crates/lash-core/src/tool_provider/triggers.rs'
      )
      ;;
    effect-host)
      area_mutation_file_args=(
        --file 'crates/lash-core/src/runtime/effect/*.rs'
        --file 'crates/lash-core/src/testing/conformance/await_event_cold.rs'
        --file 'crates/lash-core/src/testing/conformance/effect_host.rs'
      )
      ;;
    provider)
      area_mutation_file_args=(
        --file 'crates/lash-core/src/direct.rs'
        --file 'crates/lash-core/src/model.rs'
        --file 'crates/lash-core/src/llm/*.rs'
        --file 'crates/lash-core/src/provider/*.rs'
      )
      ;;
  esac
fi
# The two micro lanes (deterministic sim unit/oracle suite + perf-guard
# identity checks) share one shard: sequentially they finish well under the
# fault-matrix lane, so a separate runner each just burned scheduling overhead.
fast_shards=(
  scenario-harnesses
  fault-matrix
  sim-unit-perf-guards
  sim-generated
  minimizer-fixtures
)
SIM_SEARCH_MIN_SEEDS=4
SIM_SEARCH_MIN_MAX_BOUNDARIES=256
case "$lane" in
  fast) default_mutation_scope="none" ;;
  default) default_mutation_scope="targeted" ;;
  broad) default_mutation_scope="targeted" ;;
  full) default_mutation_scope="full" ;;
  *) default_mutation_scope="none" ;;
esac
mutation_scope="${LASH_CONFIDENCE_MUTATION_SCOPE:-$default_mutation_scope}"
coverage_scope="${LASH_CONFIDENCE_COVERAGE_SCOPE:-run}"

derive_mutation_jobs() {
  local cpu_count="${1:-}"
  if [ -z "$cpu_count" ]; then
    cpu_count="$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || printf '1')"
  fi
  if ! [[ "$cpu_count" =~ ^[1-9][0-9]*$ ]]; then
    cpu_count=1
  fi

  # Leave at least two logical CPUs per cargo-mutants job. More than four
  # concurrent Rust builds increases disk/memory pressure without improving
  # useful throughput on the CI and development machines this lane targets.
  local jobs=$(((cpu_count + 1) / 2))
  if ((jobs > 4)); then
    jobs=4
  fi
  printf '%s\n' "$jobs"
}

mutation_jobs="${LASH_MUTATION_JOBS:-$(derive_mutation_jobs)}"
if ! [[ "$mutation_jobs" =~ ^[1-9][0-9]*$ ]]; then
  echo "LASH_MUTATION_JOBS must be a positive integer, got: ${mutation_jobs}" >&2
  exit 2
fi
mutation_failures=0
mutation_postgres_container=""
mutation_postgres_database_url=""
script_started_at="$SECONDS"
current_step=""
current_step_started_at=0
mutation_commands_run=0

step() {
  if [ -n "$current_step" ]; then
    printf '    completed in %ss\n' "$((SECONDS - current_step_started_at))"
  fi
  current_step="$*"
  current_step_started_at="$SECONDS"
  printf '\n==> %s\n' "$*"
  printf '    started_at: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}

finish_current_step() {
  if [ -n "$current_step" ]; then
    printf '    completed in %ss\n' "$((SECONDS - current_step_started_at))"
    current_step=""
  fi
}

assert_no_panics_in_artifacts() {
  if [ -d "$out_dir" ] && grep -RFn --include='*.log' 'panicked at' "$out_dir" >&2; then
    echo "panic gate: FAILED ('panicked at' found in confidence artifacts)" >&2
    return 1
  fi
  echo "panic gate: clean (no 'panicked at' lines in confidence artifacts)"
}

cleanup_mutation_postgres() {
  if [ -n "$mutation_postgres_container" ]; then
    docker rm -f "$mutation_postgres_container" >/dev/null 2>&1 || true
    mutation_postgres_container=""
    mutation_postgres_database_url=""
  fi
}

finish_confidence_gate() {
  cleanup_mutation_postgres
  finish_current_step
  lash_gate_cleanup
}

trap finish_confidence_gate EXIT

usage() {
  cat <<'USAGE'
Usage: scripts/confidence-gate.sh [--dry-run] [<lane-or-shard>[+area:<surface>]]

Selectors:
  Lanes:       fast, default, broad, full
  Fast shards: fast:scenario-harnesses, fast:fault-matrix,
               fast:sim-unit-perf-guards, fast:sim-generated,
               fast:minimizer-fixtures, fast:summary
  Full shards: sim-search:<i>/<n>
  Areas:       store, process, trigger, effect-host, protocol, provider, sim

Composition:
  Append exactly one area to a depth lane, for example fast+area:store or
  full+area:effect-host. The depth still controls budgets and mutation scope;
  the area explicitly filters conformance, differential, coverage, and
  mutation work to that surface. Existing unscoped lanes retain their full
  effective scope. Fast shards may also be area-qualified when the shard owns
  that surface, for example fast:fault-matrix+area:trigger. sim-search is a
  full-depth shard and may only compose with area:sim.
  Area-qualified artifacts are isolated below the selected lane or shard at
  areas/<surface>/, so they cannot overwrite evidence from an unscoped run.

  --dry-run validates the selector and prints the execution plan without
  creating artifacts, bootstrapping tools, starting containers, or running
  commands.

Lanes:
  fast     deterministic scenario harnesses, state-machine/property checks,
           generated DST replay/provider proof, durable fault-matrix metadata,
           and perf guard identity tests.
  default  fast + focused generated SQLite seed-tail repro, local backend
           conformance, coverage blind-spot artifacts, and targeted
           cargo-mutants evidence.
  broad    bounded broad evidence: default + Postgres conformance when
           available, static model replay evidence for generated/minimized
           traces, backend contention evidence, and targeted mutation. This is
           not true full confidence.
  full     true full confidence: broad semantics plus full cargo-mutants over
           critical crates. The full lane refuses non-full mutation scopes.

  Tool policy:
  default/broad/full require cargo-llvm-cov and cargo-mutants for their
           configured mutation scope. Set LASH_CONFIDENCE_MUTATION_SCOPE for
           default/broad only; true full always requires scope=full.
          Set LASH_CONFIDENCE_COVERAGE_SCOPE=none for bounded default/broad
          replay/backend lanes that must record coverage as not_run rather than
          install cargo-llvm-cov. True full always requires coverage.
  Set LASH_CONFIDENCE_BOOTSTRAP=1 to install pinned versions if missing.
  Missing required tools fail the lane; skipped coverage or mutation shards are
  recorded as not_run, never as passed.

Fast shards:
  fast:scenario-harnesses
  fast:fault-matrix
  fast:sim-unit-perf-guards
  fast:sim-generated
  fast:minimizer-fixtures
  fast:summary

  `fast` runs all fast shards sequentially and then runs fast:summary. CI runs
  these shards as independent jobs and then runs fast:summary after downloading
  the shard artifacts.

Sim search shards:
  sim-search:<i>/<n> runs only the deterministic simulation search lane at
  full-lane budgets for one seed-index shard, writing artifacts under
  target/confidence/<worktree-slug>/sim-search/<i>-of-<n>/ locally. CI pins
  LASH_CONFIDENCE_OUT_DIR to target/confidence so the weekly Confidence workflow
  partitions the full search seed space as shard 1/<n> on the main full job
  plus matrix jobs for the remaining shards, so the union covers every seed
  exactly once.
USAGE
}

area_selected() {
  schedule_has_area "$1" && schedule_row_matches_area "$1"
}

# Each row is selector|area|suite|plan description|artifact_key=relative_path,...
# The selector is the effective lane plus fast shard; `all` is the unscoped
# area. Consumers below deliberately query this table instead of maintaining
# another lane/area membership list.
confidence_schedule_table=(
  "fast:all|store|scenario-harnesses|store contracts and SQLite substrate conformance|"
  "fast:all|process|scenario-harnesses|runtime persistence, session graph, runtime, and agent scenarios|"
  "fast:all|protocol|scenario-harnesses|Standard and RLM protocol scenarios|"
  "fast:all|process|fault-matrix|runtime state machine and durable process fault matrix|"
  "fast:all|trigger|fault-matrix|trigger delivery fault matrix|"
  "fast:all|effect-host|fault-matrix|inline await-event cancellation conformance|"
  "fast:all|protocol|fault-matrix|Lashlang property suite|"
  "fast:all|provider|fault-matrix|transport properties and provider failure evidence|"
  "fast:all|store|fault-matrix|SQLite backend fault-matrix conformance|"
  "fast:all|sim|sim-unit-perf-guards|simulation unit/oracle and performance-guard identity suites|"
  "fast:all|sim|sim-generated|generated deterministic simulation and minimizer evidence|sim_summary=sim/summary.json,provider_transport_exclusions=sim/provider-transport-exclusions.json,failing_minimizer_fixtures=sim/failing-minimizer-fixtures.json"
  "fast:all|sim|minimizer-fixtures|simulation minimizer fixtures|"
  "fast:scenario-harnesses|store|scenario-harnesses|store contracts and SQLite substrate conformance|sqlite_substrate_faults=sim/sqlite-substrate-faults/sqlite-faults.json"
  "fast:scenario-harnesses|process|scenario-harnesses|runtime persistence, session graph, runtime, and agent scenarios|"
  "fast:scenario-harnesses|protocol|scenario-harnesses|Standard and RLM protocol scenarios|"
  "fast:fault-matrix|process|fault-matrix|runtime state machine and durable process fault matrix|"
  "fast:fault-matrix|trigger|fault-matrix|trigger delivery fault matrix|"
  "fast:fault-matrix|effect-host|fault-matrix|inline await-event cancellation conformance|"
  "fast:fault-matrix|protocol|fault-matrix|Lashlang property suite|"
  "fast:fault-matrix|provider|fault-matrix|transport properties and provider failure evidence|"
  "fast:fault-matrix|store|fault-matrix|SQLite backend fault-matrix conformance|"
  "fast:sim-unit-perf-guards|sim|sim-unit-perf-guards|simulation unit/oracle and performance-guard identity suites|"
  "fast:sim-generated|sim|sim-generated|generated deterministic simulation lane|sim_summary=sim/summary.json,provider_transport_exclusions=sim/provider-transport-exclusions.json,env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,restate_postgres_workers_e2e=sim/restate-postgres-workers-e2e.json"
  "fast:minimizer-fixtures|sim|minimizer-fixtures|simulation minimizer fixtures|failing_minimizer_fixtures=sim/failing-minimizer-fixtures.json"
  "fast:summary|all|summary|validate all unscoped fast shard summaries|"
  "sim-search|sim|sim-search|deterministic simulation search shard at full budgets|"
  "default|store|scenario-harnesses|store contracts, SQLite faults, local backend conformance, contention, and Postgres replay|focused_sqlite_seed_tail_repro=sim/focused-sqlite-seed-tail/focused-sqlite-seed-tail.json,backend_contention=sim/backend-contention/backend-contention.json,postgres_current_trace_replay=sim/postgres-current/status.json,postgres_current_trace_replay_report=sim/postgres-replay/postgres-replay.json,env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "default|process|scenario-harnesses|runtime persistence, session graph, runtime scenarios, and process fault matrix|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,restate_postgres_workers_e2e=sim/restate-postgres-workers-e2e.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "default|trigger|fault-matrix|trigger delivery fault matrix|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "default|effect-host|fault-matrix|inline await-event cancellation conformance|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "default|protocol|scenario-harnesses|protocol scenarios and property suites|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "default|provider|fault-matrix|provider transport, failure, and exclusion evidence|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "default|sim|simulation|simulation unit, generated, search, minimizer, and replay evidence|sim_summary=sim/summary.json,sim_search_run=sim/search.json,provider_transport_exclusions=sim/provider-transport-exclusions.json,failing_minimizer_fixtures=sim/failing-minimizer-fixtures.json,env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "broad|store|scenario-harnesses|store contracts, SQLite faults, local backend conformance, contention, and Postgres replay|focused_sqlite_seed_tail_repro=sim/focused-sqlite-seed-tail/focused-sqlite-seed-tail.json,backend_contention=sim/backend-contention/backend-contention.json,generated_postgres_dynamic_replay=sim/postgres-generated-rerun/summary.json,env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "broad|store|postgres-conformance|bounded Postgres conformance and dynamic backend differential|generated_postgres_dynamic_replay=sim/postgres-generated-rerun/summary.json"
  "broad|process|scenario-harnesses|runtime persistence, session graph, runtime scenarios, and process fault matrix|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,restate_postgres_workers_e2e=sim/restate-postgres-workers-e2e.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "broad|trigger|fault-matrix|trigger delivery fault matrix|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "broad|effect-host|fault-matrix|inline await-event cancellation conformance|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "broad|protocol|scenario-harnesses|protocol scenarios and property suites|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "broad|provider|fault-matrix|provider transport, failure, and exclusion evidence|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "broad|sim|simulation|simulation unit, generated, search, minimizer, and replay evidence|sim_summary=sim/summary.json,sim_search_run=sim/search.json,provider_transport_exclusions=sim/provider-transport-exclusions.json,failing_minimizer_fixtures=sim/failing-minimizer-fixtures.json,env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "broad|all|model-replay|model replay evidence|model_replay_evidence=sim/model-replay/summary.json"
  "full|store|scenario-harnesses|store contracts, SQLite faults, local backend conformance, contention, and Postgres replay|focused_sqlite_seed_tail_repro=sim/focused-sqlite-seed-tail/focused-sqlite-seed-tail.json,backend_contention=sim/backend-contention/backend-contention.json,generated_postgres_dynamic_replay=sim/postgres-generated-rerun/summary.json,env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "full|store|postgres-conformance|full Postgres conformance and dynamic backend differential|generated_postgres_dynamic_replay=sim/postgres-generated-rerun/summary.json"
  "full|process|scenario-harnesses|runtime persistence, session graph, runtime scenarios, and process fault matrix|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,restate_postgres_workers_e2e=sim/restate-postgres-workers-e2e.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "full|process|restate-workers|Restate/Postgres/MinIO worker e2e|restate_postgres_workers_e2e=sim/restate-postgres-workers-e2e.json"
  "full|trigger|fault-matrix|trigger delivery fault matrix|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "full|effect-host|fault-matrix|inline await-event cancellation conformance|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "full|protocol|scenario-harnesses|protocol scenarios and property suites|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "full|provider|fault-matrix|provider transport, failure, and exclusion evidence|env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "full|sim|simulation|simulation unit, generated, search, minimizer, and replay evidence|sim_summary=sim/summary.json,sim_search_run=sim/search.json,provider_transport_exclusions=sim/provider-transport-exclusions.json,failing_minimizer_fixtures=sim/failing-minimizer-fixtures.json,env_gated_lanes=sim/env-gated-lanes.json,full_lane_prerequisites=sim/full-lane-prerequisites.json,postgres_effect_history_status=sim/postgres-effect-history-status.json,coverage_summary=coverage/summary.json,mutation_evidence=mutation-evidence.json"
  "full|all|model-replay|model replay evidence|model_replay_evidence=sim/model-replay/summary.json"
)

schedule_selector() {
  if [ -n "$sim_search_shard" ]; then
    printf 'sim-search\n'
  elif [ "$lane" = "fast" ]; then
    printf 'fast:%s\n' "$fast_shard"
  else
    printf '%s\n' "$lane"
  fi
}

schedule_row_matches_area() {
  local row_area="$1"
  [ "$area" = "all" ] || [ "$row_area" = "$area" ]
}

schedule_has_area() {
  local wanted_area="$1" selector row row_area suite description artifacts
  selector="$(schedule_selector)"
  for row in "${confidence_schedule_table[@]}"; do
    IFS='|' read -r row_selector row_area suite description artifacts <<<"$row"
    if [ "$row_selector" = "$selector" ] && [ "$row_area" = "$wanted_area" ]; then
      return 0
    fi
  done
  return 1
}

schedule_lane_fallback_reason() {
  if [ "$area" = "all" ]; then
    printf 'not_in_%s_lane\n' "$lane"
  else
    printf 'not_in_%s_lane_or_area\n' "$lane"
  fi
}

schedule_has_artifact() {
  local key="$1" selector row row_selector row_area suite description artifacts declaration
  selector="$(schedule_selector)"
  for row in "${confidence_schedule_table[@]}"; do
    IFS='|' read -r row_selector row_area suite description artifacts <<<"$row"
    [ "$row_selector" = "$selector" ] || continue
    schedule_row_matches_area "$row_area" || continue
    for declaration in ${artifacts//,/ }; do
      [ "${declaration%%=*}" = "$key" ] && return 0
    done
  done
  return 1
}

scheduled_artifact_path() {
  local key="$1" path="$2" fallback="$3"
  if schedule_has_artifact "$key"; then
    printf '%s\n' "$path"
  else
    printf '%s\n' "$fallback"
  fi
}

scheduled_existing_artifact_path() {
  local key="$1" path="$2" fallback="$3"
  if schedule_has_artifact "$key" && [ -f "${out_dir}/${path}" ]; then
    printf '%s\n' "$path"
  else
    printf '%s\n' "$fallback"
  fi
}

write_confidence_prerequisite_failure() {
  local prerequisite="$1"
  local detail="$2"
  local install_command="$3"
  local failure_dir="${out_dir}/prerequisites"
  local failure_file="${failure_dir}/${prerequisite}.json"
  mkdir -p "$failure_dir"
  cat >"$failure_file" <<EOF
{
  "schema": "lash.confidence.prerequisite-failure.v1",
  "lane": "${lane}",
  "status": "failed",
  "prerequisite": "${prerequisite}",
  "detail": "${detail}",
  "install_command": "${install_command}",
  "bootstrap_command": "LASH_CONFIDENCE_BOOTSTRAP=1 LASH_CONFIDENCE_OUT_DIR=${out_root} scripts/confidence-gate.sh ${requested_selector}",
  "exact_retry_command": "LASH_CONFIDENCE_OUT_DIR=${out_root} scripts/confidence-gate.sh ${requested_selector}"
}
EOF
  cat >"${out_dir}/confidence-summary.json" <<EOF
{
  "schema": "lash.confidence.summary.v1",
  "lane": "${lane}",
  "status": "failed",
  "failure_kind": "missing_prerequisite",
  "prerequisite_failure": "prerequisites/${prerequisite}.json",
  "sim_summary": "$([ -f "${out_dir}/sim/summary.json" ] && echo "sim/summary.json" || echo "not_written")",
  "env_gated_lanes": "$([ -f "${out_dir}/sim/env-gated-lanes.json" ] && echo "sim/env-gated-lanes.json" || echo "not_written")",
  "full_lane_prerequisites": "$([ -f "${out_dir}/sim/full-lane-prerequisites.json" ] && echo "sim/full-lane-prerequisites.json" || echo "not_written")",
  "mutation_evidence": "$([ -f "${out_dir}/mutation-evidence.json" ] && echo "mutation-evidence.json" || echo "not_reached")",
  "artifacts_root": "${out_dir}"
}
EOF
}

case "$lane" in
  fast|default|broad|full) ;;
  -h|--help)
    usage
    exit 0
    ;;
  *)
    echo "Unknown confidence lane or shard '${requested_lane}'." >&2
    usage >&2
    exit 2
    ;;
esac

if [ -n "$sim_search_shard" ]; then
  if ! [[ "$sim_search_shard" =~ ^([1-9][0-9]*)/([1-9][0-9]*)$ ]] \
    || ((10#${BASH_REMATCH[1]} > 10#${BASH_REMATCH[2]})); then
    echo "Invalid sim-search shard '${sim_search_shard}'; expected sim-search:<i>/<n> with 1 <= i <= n." >&2
    usage >&2
    exit 2
  fi
fi

if [ "$lane" = "fast" ]; then
  case "$fast_shard" in
    all|summary|scenario-harnesses|fault-matrix|sim-unit-perf-guards|sim-generated|minimizer-fixtures) ;;
    *)
      echo "Unknown fast shard '${fast_shard}'." >&2
      usage >&2
      exit 2
      ;;
  esac
elif [ "$fast_shard" != "all" ]; then
  usage >&2
  exit 2
fi

if [ "$lane" = "fast" ] && [ "$area" != "all" ]; then
  if ! schedule_has_area "$area"; then
    echo "${requested_lane} has no area:${area} work." >&2
    exit 2
  fi
fi

if [ "$dry_run" -eq 0 ] && [ "$lane" != "fast" ]; then
  lash_gate_acquire "confidence-${requested_selector}"
fi
if [ "$dry_run" -eq 0 ]; then
  mkdir -p "$out_dir"
fi

case "$coverage_scope" in
  run|none) ;;
  *)
    echo "Unknown LASH_CONFIDENCE_COVERAGE_SCOPE=${coverage_scope}; expected run or none" >&2
    exit 2
    ;;
esac

if [ "$dry_run" -eq 1 ] && [ "$lane" = "full" ] && [ "$mutation_scope" != "full" ]; then
  echo "The full lane requires LASH_CONFIDENCE_MUTATION_SCOPE=full." >&2
  exit 2
fi
if [ "$dry_run" -eq 1 ] && [ "$lane" = "full" ] && [ "$coverage_scope" != "run" ]; then
  echo "The full lane requires coverage." >&2
  exit 2
fi

if [ "$lane" = "full" ] && [ "$mutation_scope" != "full" ]; then
  cat >"${out_dir}/confidence-summary.json" <<EOF
{
  "schema": "lash.confidence.summary.v1",
  "lane": "full",
  "status": "failed",
  "failure_kind": "invalid_full_lane_mutation_scope",
  "mutation_scope": "${mutation_scope}",
  "reason": "true full confidence may only pass when LASH_CONFIDENCE_MUTATION_SCOPE=full and full cargo-mutants runs complete",
  "bounded_alternative": "LASH_CONFIDENCE_OUT_DIR=${out_root} scripts/confidence-gate.sh broad",
  "artifacts_root": "${out_dir}"
}
EOF
  echo "The full lane requires LASH_CONFIDENCE_MUTATION_SCOPE=full. Use the broad lane for bounded targeted evidence." >&2
  exit 2
fi

if [ "$lane" = "full" ] && [ "$coverage_scope" != "run" ]; then
  cat >"${out_dir}/confidence-summary.json" <<EOF
{
  "schema": "lash.confidence.summary.v1",
  "lane": "full",
  "status": "failed",
  "failure_kind": "invalid_full_lane_coverage_scope",
  "coverage_scope": "${coverage_scope}",
  "reason": "true full confidence must run coverage; LASH_CONFIDENCE_COVERAGE_SCOPE=none is only for bounded default/broad replay/backend lanes",
  "bounded_alternative": "LASH_CONFIDENCE_COVERAGE_SCOPE=none LASH_CONFIDENCE_MUTATION_SCOPE=none LASH_CONFIDENCE_OUT_DIR=${out_root} scripts/confidence-gate.sh broad",
  "artifacts_root": "${out_dir}"
}
EOF
  echo "The full lane requires coverage. Use a bounded broad lane for replay/backend evidence without coverage." >&2
  exit 2
fi

bootstrap_tools() {
  if [ "${LASH_CONFIDENCE_BOOTSTRAP:-0}" != "1" ]; then
    return
  fi
  if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    step "Bootstrap cargo-llvm-cov 0.8.7"
    cargo install cargo-llvm-cov --version 0.8.7 --locked
  fi
  if ! command -v cargo-mutants >/dev/null 2>&1; then
    step "Bootstrap cargo-mutants 27.1.0"
    cargo install cargo-mutants --version 27.1.0 --locked
  fi
  if command -v rustup >/dev/null 2>&1 \
    && ! rustup component list --installed | grep -Eq '^llvm-tools-preview($|-)'; then
    step "Bootstrap rustup component llvm-tools-preview"
    rustup component add llvm-tools-preview
  fi
  if [ -z "${LLVM_COV:-}" ] && [ -z "${LLVM_PROFDATA:-}" ] \
    && ! command -v rustup >/dev/null 2>&1 \
    && command -v nix >/dev/null 2>&1; then
    bootstrap_nix_llvm_tools
  fi
}

require_tool() {
  local tool="$1"
  local crate="$2"
  local version="$3"
  if command -v "$tool" >/dev/null 2>&1; then
    return
  fi
  cat >&2 <<EOF
Required tool '$tool' is not installed for the '$lane' confidence lane.
Install with:
  cargo install ${crate} --version ${version} --locked
or rerun with:
  LASH_CONFIDENCE_BOOTSTRAP=1 scripts/confidence-gate.sh ${requested_selector}
EOF
  write_confidence_prerequisite_failure \
    "$tool" \
    "missing_required_tool_${tool}" \
    "cargo install ${crate} --version ${version} --locked"
  exit 127
}

run_mutants_recorded() {
  local name="$1"
  local artifact="$2"
  shift 2
  mutation_commands_run=$((${mutation_commands_run:-0} + 1))
  mkdir -p "$artifact"
  set +e
  # cargo-mutants creates one scratch/build directory per concurrent job. Keep
  # Cargo's target relative to each scratch tree: an inherited absolute target
  # (from either the environment or global Cargo config) collapses the targets
  # back together and lets mutated source trees race over the same artifacts.
  CARGO_TARGET_DIR=target "$@"
  local exit_code=$?
  set -e
  local status
  if [ "$exit_code" -eq 0 ]; then
    status="passed"
  else
    status="failed"
    mutation_failures=$((mutation_failures + 1))
  fi
  cat >"${artifact}/confidence-status.json" <<EOF
{
  "schema": "lash.confidence.mutation-command-status.v1",
  "name": "${name}",
  "status": "${status}",
  "exit_code": ${exit_code},
  "scope": "${mutation_scope}"
}
EOF
}

start_mutation_postgres() {
  local artifact="$1"
  local port deadline
  if ! command -v docker >/dev/null 2>&1; then
    echo "Postgres mutation requires Docker for an isolated ephemeral database." >&2
    exit 127
  fi

  cleanup_mutation_postgres
  mutation_postgres_container="lash-confidence-mutation-postgres-${LASH_GATE_WORKTREE_SLUG}-$(basename "$artifact")-$$"
  bash scripts/docker-pull-with-retry.sh postgres:16-alpine
  docker run -d --name "$mutation_postgres_container" \
    --label "$LASH_GATE_LABEL" \
    --network "$LASH_E2E_NETWORK" \
    -e POSTGRES_USER=lash \
    -e POSTGRES_PASSWORD=lash \
    -e POSTGRES_DB=lash \
    -p "127.0.0.1:${LASH_CONFIDENCE_MUTATION_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 12))}:5432" \
    postgres:16-alpine >/dev/null

  port="$(
    docker inspect \
      --format '{{(index (index .NetworkSettings.Ports "5432/tcp") 0).HostPort}}' \
      "$mutation_postgres_container"
  )"
  deadline=$((SECONDS + 60))
  until docker exec "$mutation_postgres_container" pg_isready -U lash -d lash >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$mutation_postgres_container" >&2 || true
      echo "Mutation Postgres did not become ready on port ${port}" >&2
      exit 1
    fi
    sleep 1
  done
  mutation_postgres_database_url="postgres://lash:lash@127.0.0.1:${port}/lash"
}

run_postgres_mutants_recorded() {
  local name="$1"
  local artifact="$2"
  shift 2
  start_mutation_postgres "$artifact"
  LASH_POSTGRES_DATABASE_URL="$mutation_postgres_database_url" \
    LASH_REQUIRE_POSTGRES=1 \
    run_mutants_recorded "$name" "$artifact" "$@" --jobs "$mutation_jobs"
  cleanup_mutation_postgres
}

bootstrap_nix_llvm_tools() {
  local llvm_major llvm_attr llvm_out
  llvm_major="$(
    rustc -vV \
      | awk -F': ' '/^LLVM version:/ { split($2, version, "."); print version[1] }'
  )"
  if [ -z "$llvm_major" ]; then
    echo "Could not infer LLVM major version from rustc -vV" >&2
    exit 127
  fi

  llvm_attr="nixpkgs#llvmPackages_${llvm_major}.llvm"
  step "Bootstrap ${llvm_attr}"
  llvm_out="$(
    nix build --no-link --print-out-paths "$llvm_attr" \
      --extra-experimental-features 'nix-command flakes'
  )"
  export LLVM_COV="${llvm_out}/bin/llvm-cov"
  export LLVM_PROFDATA="${llvm_out}/bin/llvm-profdata"
  if [ ! -x "$LLVM_COV" ] || [ ! -x "$LLVM_PROFDATA" ]; then
    echo "Nix LLVM package did not provide executable llvm-cov/llvm-profdata under ${llvm_out}/bin" >&2
    exit 127
  fi
}

require_llvm_tools() {
  if [ -n "${LLVM_COV:-}" ] && [ -n "${LLVM_PROFDATA:-}" ]; then
    return
  fi
  if command -v rustup >/dev/null 2>&1 \
    && rustup component list --installed | grep -Eq '^llvm-tools-preview($|-)'; then
    return
  fi
  if [ "${LASH_CONFIDENCE_BOOTSTRAP:-0}" = "1" ] \
    && ! command -v rustup >/dev/null 2>&1 \
    && command -v nix >/dev/null 2>&1; then
    bootstrap_nix_llvm_tools
    return
  fi
  if ! command -v rustup >/dev/null 2>&1; then
    cat >&2 <<EOF
Coverage requires llvm-tools-preview, or explicit LLVM_COV and LLVM_PROFDATA paths.
This environment does not have rustup, so the gate cannot bootstrap the Rust
llvm-tools component here.

Set compatible binaries explicitly:
  LLVM_COV=/path/to/llvm-cov LLVM_PROFDATA=/path/to/llvm-profdata

Or let the gate build the matching Nix LLVM package inferred from rustc -vV:
  LASH_CONFIDENCE_BOOTSTRAP=1 scripts/confidence-gate.sh ${requested_selector}
EOF
    write_confidence_prerequisite_failure \
      "llvm-tools" \
      "missing_llvm_tools_without_rustup" \
      "LLVM_COV=/path/to/llvm-cov LLVM_PROFDATA=/path/to/llvm-profdata"
    exit 127
  fi
  cat >&2 <<EOF
Coverage requires llvm-tools-preview, or explicit LLVM_COV and LLVM_PROFDATA paths.
Install with:
  rustup component add llvm-tools-preview
or rerun with:
  LASH_CONFIDENCE_BOOTSTRAP=1 scripts/confidence-gate.sh ${requested_selector}
If rustup is unavailable but Nix is installed, the bootstrap path builds
nixpkgs#llvmPackages_\${rustc_llvm_major}.llvm and exports LLVM_COV/LLVM_PROFDATA.
EOF
  write_confidence_prerequisite_failure \
    "llvm-tools" \
    "missing_llvm_tools_preview_component" \
    "rustup component add llvm-tools-preview"
  exit 127
}

run_cargo_tests() {
  if cargo nextest --version >/dev/null 2>&1; then
    cargo nextest run "$@"
  else
    cargo test "$@"
  fi
}

run_scenario_harnesses() {
  local default_store_contract_cases=32
  local default_runtime_persistence_cases=32
  local default_session_graph_cases=24
  if [ "$lane" = "full" ]; then
    default_store_contract_cases=256
    default_runtime_persistence_cases=256
    default_session_graph_cases=256
  fi
  local store_contract_cases="${LASH_STORE_CONTRACT_PROPTEST_CASES:-$default_store_contract_cases}"
  local runtime_persistence_cases="${LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES:-$default_runtime_persistence_cases}"
  local session_graph_cases="${LASH_SESSION_GRAPH_PROPTEST_CASES:-$default_session_graph_cases}"

  if area_selected store; then
    step "Golden durable-store semantic read-back"
    run_cargo_tests -p lash-internal-sqlite-store --locked --test durable_read_fixture \
      sqlite_durable_fixture_reads_with_identical_semantics
    run_cargo_tests -p lash-internal-postgres-store --locked --test durable_read_fixture \
      postgres_durable_fixture_reads_with_identical_semantics_when_configured

    step "Durable store-contract state-machine properties"
    LASH_STORE_CONTRACT_PROPTEST_CASES="$store_contract_cases" \
      run_cargo_tests -p lash-internal-core --locked store_contract_state_machine_properties
    LASH_STORE_CONTRACT_PROPTEST_CASES="$store_contract_cases" \
      run_cargo_tests -p lash-internal-sqlite-store --locked --test conformance \
      store_contract_state_machine_properties
    LASH_STORE_CONTRACT_PROPTEST_CASES="$store_contract_cases" \
      run_cargo_tests -p lash-internal-postgres-store --locked --test conformance \
      store_contract_state_machine_properties_when_configured

    local sqlite_fault_seeds=4
    if [ "$lane" = "full" ]; then
      sqlite_fault_seeds=256
    fi
    sqlite_fault_seeds="${LASH_SQLITE_FAULT_SEEDS:-$sqlite_fault_seeds}"
    step "Real SQLite substrate faults (${sqlite_fault_seeds} deterministic seeds)"
    cargo run -p lash-sim --locked -- sqlite-faults \
      --out "${out_dir}/sim/sqlite-substrate-faults" \
      --seeds "$sqlite_fault_seeds"
  fi

  if area_selected process; then
    step "Runtime-persistence state-machine properties"
    LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES="$runtime_persistence_cases" \
      run_cargo_tests -p lash-internal-core --locked runtime_persistence_state_machine_properties
    LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES="$runtime_persistence_cases" \
      run_cargo_tests -p lash-internal-sqlite-store --locked --test conformance \
      runtime_persistence_state_machine_properties
    LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES="$runtime_persistence_cases" \
      run_cargo_tests -p lash-internal-postgres-store --locked --test conformance \
      runtime_persistence_state_machine_properties_when_configured

    step "Session graph state-machine property harness"
    LASH_SESSION_GRAPH_PROPTEST_CASES="$session_graph_cases" \
      run_cargo_tests -p lash-internal-core --locked session_graph_state_machine_properties
    LASH_SESSION_GRAPH_PROPTEST_CASES="$session_graph_cases" \
      run_cargo_tests -p lash-internal-sqlite-store --locked --test conformance \
      session_graph_state_machine_properties
    LASH_SESSION_GRAPH_PROPTEST_CASES="$session_graph_cases" \
      run_cargo_tests -p lash-internal-postgres-store --locked --test conformance \
      session_graph_state_machine_properties_when_configured

    step "Runtime Scenario harness"
    run_cargo_tests -p lash-internal-core --locked runtime_scenario

    step "Agent Scenario harness"
    run_cargo_tests -p lash-runtime --locked --features rlm,testing agent_scenarios
    run_cargo_tests -p lash-runtime --locked --features rlm,testing agent_scenario_contract_metadata
  fi

  if area_selected protocol; then
    step "Standard Protocol Scenario harness"
    run_cargo_tests -p lash-internal-protocol-standard --locked --test protocol_scenarios
    run_cargo_tests -p lash-internal-protocol-standard --locked standard_scenario_contract_metadata

    step "RLM Protocol Scenario harness"
    run_cargo_tests -p lash-internal-protocol-rlm --locked --test protocol_drivers
    run_cargo_tests -p lash-internal-protocol-rlm --locked rlm_scenario_contract_metadata
  fi
}

run_state_machine_and_fault_matrix() {
  if area_selected process; then
    step "Runtime state-machine property runner"
    run_cargo_tests -p lash-internal-core --locked runtime_state_machine_property
    step "Durable fault matrix metadata"
    run_cargo_tests -p lash-internal-core --locked durable_fault_matrix
    step "Durable process fault-matrix evidence"
    run_cargo_tests -p lash-runtime --locked --features rlm,testing \
      runtime_rebuild_and_worker_recovery_with_durable_stores
    run_cargo_tests -p lash-internal-core --locked \
      queued_work_claims_supersede_across_session_lease_generations
    run_cargo_tests -p lash-internal-core --locked \
      turn_input_claims_supersede_across_session_lease_generations
    run_cargo_tests -p lash-internal-core --locked \
      same_generation_claim_scans_reach_rows_beyond_the_scan_surplus
  fi

  if area_selected protocol; then
    step "Lashlang property suite"
    run_cargo_tests -p lash-internal-lashlang --locked --test property
  fi

  if area_selected provider; then
    step "LLM transport SSE framing property suite"
    run_cargo_tests -p lash-internal-llm-transport --locked --test property
    run_cargo_tests -p lash-internal-provider-anthropic --locked --test property
    run_cargo_tests -p lash-internal-provider-google --locked --test property
    step "Provider retry fault-matrix evidence"
    run_cargo_tests -p lash-internal-core --locked retryable_llm_failures_exhaust_and_fail_turn
    run_cargo_tests -p lash-internal-protocol-standard --locked --test protocol_scenarios \
      standard_protocol_scenario_provider_error_stops_without_checkpoint
  fi

  if area_selected effect-host; then
    step "Native effect-host await-event session-cancel conformance"
    run_cargo_tests -p lash-internal-core --locked native_effect_host_satisfies_conformance
  fi

  if area_selected trigger; then
    step "Durable trigger fault-matrix evidence"
    run_cargo_tests -p lash-internal-core --locked sweep_reconciles_reserved_trigger_delivery_without_process
    run_cargo_tests -p lash-internal-core --locked \
      sweep_does_not_reconcile_trigger_delivery_pruned_with_terminal_process
  fi

  if area_selected store; then
    step "SQLite backend fault-matrix conformance"
    cargo test -p lash-internal-sqlite-store --locked --test conformance conformance
  fi
}

run_sim_unit_suite() {
  step "Deterministic simulation unit/oracle suite"
  run_cargo_tests -p lash-sim --locked -- \
    --skip generated_sim_profile_writes_trace_replay_and_provider_artifacts \
    --skip minimizer_preserves \
    --skip minimizer_writes_replayable_regression_package
}

run_sim_generated_lane() {
  step "Deterministic simulation generated lane"
  local sim_profile
  case "$lane" in
    fast) sim_profile="${LASH_SIM_PROFILE:-fast-random}" ;;
    default) sim_profile="${LASH_SIM_PROFILE:-default-random}" ;;
    broad) sim_profile="${LASH_SIM_PROFILE:-full-random}" ;;
    full) sim_profile="${LASH_SIM_PROFILE:-full-random}" ;;
  esac
  local cmd=(cargo run -p lash-sim --locked -- run --out "${out_dir}/sim" --profile "$sim_profile")
  if [ -n "${LASH_SIM_SEEDS:-}" ]; then
    cmd+=(--seeds "$LASH_SIM_SEEDS")
  elif [ "$lane" = "broad" ]; then
    cmd+=(--seeds "${LASH_BROAD_SIM_SEEDS:-2}")
  fi
  if [ -n "${LASH_SIM_MAX_BOUNDARIES:-}" ]; then
    cmd+=(--max-boundaries "$LASH_SIM_MAX_BOUNDARIES")
  elif [ "$lane" = "broad" ]; then
    cmd+=(--max-boundaries "${LASH_BROAD_SIM_MAX_BOUNDARIES:-128}")
  fi
  "${cmd[@]}"

  run_sim_search_lane
}

minimizer_fixture_names() {
  cat <<'EOF'
operational-coverage-missing-cancellation
scheduler-owned-provider-completion-missing-evidence
queued-input-operational-missing
trigger-wakeup-operational-missing
process-wake-operational-missing
rlm-lashlang-cell-missing-continuation
agent-parallel-join-missing-wake-session
standard-provider-error-missing-parser-matrix
standard-max-turn-stop-missing
rlm-typed-finish-terminal-event-missing
rlm-empty-options-default-mode-broken
agent-tuple-json-array-shape-broken
agent-started-process-subagent-child-graph-missing
agent-failed-child-task-fail-evidence-missing
provider-mutation-runtime-completion-missing
worker-failover-stale-rejection-missing
backend-retry-runtime-completion-missing
EOF
}

default_minimizer_fixture_jobs() {
  python3 - <<'PY'
import os

cpu_count = os.cpu_count() or 2
print(max(1, min(4, cpu_count)))
PY
}

run_minimizer_fixture_suite() {
  step "Deterministic simulation failing minimizer fixture"
  run_cargo_tests -p lash-sim --locked minimizer

  step "Build lash-sim minimizer binary"
  cargo build -p lash-sim --locked --bin lash-sim

  mkdir -p "${out_dir}/sim"
  local fixture_root="${out_dir}/sim/failing-fixtures"
  mkdir -p "$fixture_root"
  local fixture_jobs
  fixture_jobs="${LASH_MINIMIZER_FIXTURE_JOBS:-$(default_minimizer_fixture_jobs)}"
  local cargo_target_dir lash_sim_bin
  cargo_target_dir="${CARGO_TARGET_DIR:-target}"
  lash_sim_bin="${cargo_target_dir%/}/debug/lash-sim"
  if [ ! -x "$lash_sim_bin" ]; then
    echo "Expected lash-sim binary at ${lash_sim_bin}" >&2
    exit 1
  fi

  step "Generate minimized failing fixture artifacts"
  export LASH_SIM_BIN="$lash_sim_bin"
  export LASH_MINIMIZER_FIXTURE_ROOT="$fixture_root"
  minimizer_fixture_names \
    | xargs -n 1 -P "$fixture_jobs" sh -c '
        fixture="$1"
        "$LASH_SIM_BIN" minimize \
          "crates/lash-sim/failure-fixtures/${fixture}.json" \
          --out "${LASH_MINIMIZER_FIXTURE_ROOT}/${fixture}"
      ' sh

  cat >"${out_dir}/sim/failing-minimizer-fixtures.json" <<EOF
{
  "schema": "lash.confidence.failing-minimizer-fixtures.v1",
  "status": "passed",
  "parallel_jobs": ${fixture_jobs},
  "fixtures": [
    "crates/lash-sim/failure-fixtures/operational-coverage-missing-cancellation.json",
    "crates/lash-sim/failure-fixtures/scheduler-owned-provider-completion-missing-evidence.json",
    "crates/lash-sim/failure-fixtures/queued-input-operational-missing.json",
    "crates/lash-sim/failure-fixtures/trigger-wakeup-operational-missing.json",
    "crates/lash-sim/failure-fixtures/process-wake-operational-missing.json",
    "crates/lash-sim/failure-fixtures/rlm-lashlang-cell-missing-continuation.json",
    "crates/lash-sim/failure-fixtures/agent-parallel-join-missing-wake-session.json",
    "crates/lash-sim/failure-fixtures/standard-provider-error-missing-parser-matrix.json",
    "crates/lash-sim/failure-fixtures/standard-max-turn-stop-missing.json",
    "crates/lash-sim/failure-fixtures/rlm-typed-finish-terminal-event-missing.json",
    "crates/lash-sim/failure-fixtures/rlm-empty-options-default-mode-broken.json",
    "crates/lash-sim/failure-fixtures/agent-tuple-json-array-shape-broken.json",
    "crates/lash-sim/failure-fixtures/agent-started-process-subagent-child-graph-missing.json",
    "crates/lash-sim/failure-fixtures/agent-failed-child-task-fail-evidence-missing.json",
    "crates/lash-sim/failure-fixtures/provider-mutation-runtime-completion-missing.json",
    "crates/lash-sim/failure-fixtures/worker-failover-stale-rejection-missing.json",
    "crates/lash-sim/failure-fixtures/backend-retry-runtime-completion-missing.json"
  ],
  "test_filter": "minimizer",
  "preserves": "oracle_id,status,semantic_reason",
  "minimized_packages": {
    "operational_coverage_missing_cancellation": "failing-fixtures/operational-coverage-missing-cancellation/minimized-regression/package.json",
    "scheduler_owned_provider_completion_missing_evidence": "failing-fixtures/scheduler-owned-provider-completion-missing-evidence/minimized-regression/package.json",
    "queued_input_operational_missing": "failing-fixtures/queued-input-operational-missing/minimized-regression/package.json",
    "trigger_wakeup_operational_missing": "failing-fixtures/trigger-wakeup-operational-missing/minimized-regression/package.json",
    "process_wake_operational_missing": "failing-fixtures/process-wake-operational-missing/minimized-regression/package.json",
    "rlm_lashlang_cell_missing_continuation": "failing-fixtures/rlm-lashlang-cell-missing-continuation/minimized-regression/package.json",
    "agent_parallel_join_missing_wake_session": "failing-fixtures/agent-parallel-join-missing-wake-session/minimized-regression/package.json",
    "standard_provider_error_missing_parser_matrix": "failing-fixtures/standard-provider-error-missing-parser-matrix/minimized-regression/package.json",
    "standard_max_turn_stop_missing": "failing-fixtures/standard-max-turn-stop-missing/minimized-regression/package.json",
    "rlm_typed_finish_terminal_event_missing": "failing-fixtures/rlm-typed-finish-terminal-event-missing/minimized-regression/package.json",
    "rlm_empty_options_default_mode_broken": "failing-fixtures/rlm-empty-options-default-mode-broken/minimized-regression/package.json",
    "agent_tuple_json_array_shape_broken": "failing-fixtures/agent-tuple-json-array-shape-broken/minimized-regression/package.json",
    "agent_started_process_subagent_child_graph_missing": "failing-fixtures/agent-started-process-subagent-child-graph-missing/minimized-regression/package.json",
    "agent_failed_child_task_fail_evidence_missing": "failing-fixtures/agent-failed-child-task-fail-evidence-missing/minimized-regression/package.json",
    "provider_mutation_runtime_completion_missing": "failing-fixtures/provider-mutation-runtime-completion-missing/minimized-regression/package.json",
    "worker_failover_stale_rejection_missing": "failing-fixtures/worker-failover-stale-rejection-missing/minimized-regression/package.json",
    "backend_retry_runtime_completion_missing": "failing-fixtures/backend-retry-runtime-completion-missing/minimized-regression/package.json"
  }
}
EOF
}

run_sim_provider_scripts() {
  run_sim_unit_suite
  run_sim_generated_lane
  run_minimizer_fixture_suite
}

run_sim_search_lane() {
  if [ "$lane" = "fast" ]; then
    return
  fi
  mkdir -p "${out_dir}/sim"
  local search_profile search_seeds search_max_boundaries
  case "$lane" in
    default)
      search_profile="${LASH_SIM_SEARCH_PROFILE:-default-random}"
      search_seeds="${LASH_SIM_DEFAULT_SEEDS:-256}"
      search_max_boundaries="${LASH_SIM_DEFAULT_MAX_BOUNDARIES:-500}"
      ;;
    broad)
      search_profile="${LASH_SIM_SEARCH_PROFILE:-full-random}"
      search_seeds="${LASH_SIM_BROAD_SEEDS:-512}"
      search_max_boundaries="${LASH_SIM_BROAD_MAX_BOUNDARIES:-512}"
      ;;
    full)
      search_profile="${LASH_SIM_SEARCH_PROFILE:-full-random}"
      search_seeds="${LASH_SIM_FULL_SEEDS:-5000}"
      search_max_boundaries="${LASH_SIM_FULL_MAX_BOUNDARIES:-2000}"
      ;;
  esac
  local search_shard="${LASH_SIM_SHARD:-1/1}"
  step "Deterministic simulation search lane (${search_seeds} seeds @ ${search_max_boundaries} max boundaries, shard ${search_shard})"
  local search_dir="${out_dir}/sim-search"
  local search_salt="${LASH_SIM_RUN_SALT:-}"
  local salt_args=()
  if [ -n "$search_salt" ]; then
    salt_args+=(--salt "$search_salt")
  fi
  cargo run -p lash-sim --locked -- run \
    --out "$search_dir" \
    --profile "$search_profile" \
    --seeds "$search_seeds" \
    --max-boundaries "$search_max_boundaries" \
    --shard "$search_shard" \
    --mode search \
    "${salt_args[@]}"
  python3 - "${search_dir}/summary.json" "${out_dir}/sim/search.json" "$search_max_boundaries" "$SIM_SEARCH_MIN_SEEDS" "$SIM_SEARCH_MIN_MAX_BOUNDARIES" <<'PY'
import json
import sys

summary_path, output_path, max_boundaries, min_seeds, min_max_boundaries = sys.argv[1:6]
with open(summary_path, "r", encoding="utf-8") as handle:
    summary = json.load(handle)
counts = summary.get("counts") or {}
min_seeds = int(min_seeds)
min_max_boundaries = int(min_max_boundaries)
artifact = {
    "schema": "lash.confidence.sim-search-run.v1",
    "status": "passed",
    "mode": summary.get("mode"),
    "profile": summary.get("profile"),
    "shard": summary.get("shard"),
    "configured_seeds": summary.get("configured_seeds"),
    "configured_max_boundaries": int(max_boundaries),
    "required_min_seeds": min_seeds,
    "required_min_max_boundaries": min_max_boundaries,
    "summary_path": summary_path,
    "counts": {
        "generated_seeds": counts.get("generated_seeds"),
        "boundary_events": counts.get("boundary_events"),
        "oracle_passes": counts.get("oracle_passes"),
        "oracle_failures": counts.get("oracle_failures"),
        "scheduler_owned_runtime_completions": counts.get("scheduler_owned_runtime_completions"),
        "interleaving_depth_max": counts.get("interleaving_depth_max"),
        "interleaving_depth_min": counts.get("interleaving_depth_min"),
    },
    "semantics": "high-volume seed search over the generated DST world in search mode: every seed runs live with the full oracle set plus an in-memory determinism replay, and failures persist complete reproducibility packages under sim-search/failures/; per-seed evidence artifacts stay in the bounded evidence lane",
}
required_interleaving_depth = 2
errors = []
if summary.get("mode") != "search":
    errors.append("sim search lane must run in search mode")
if counts.get("generated_seeds", 0) < min_seeds:
    errors.append(f"sim search run must execute at least {min_seeds} generated seeds in this shard")
if int(max_boundaries) < min_max_boundaries:
    errors.append(f"sim search run must configure at least {min_max_boundaries} max boundaries")
if counts.get("boundary_events", 0) < 512:
    errors.append("sim search run produced fewer than 512 boundary events")
if counts.get("oracle_failures", 1) != 0:
    errors.append("sim search run had oracle failures")
if counts.get("interleaving_depth_max", 0) < required_interleaving_depth:
    errors.append(
        "sim search run never interleaved >= "
        f"{required_interleaving_depth} live provider turns "
        f"(peak {counts.get('interleaving_depth_max', 0)}); the scheduler is not exercising concurrency"
    )
if errors:
    artifact["status"] = "failed"
    artifact["errors"] = errors
with open(output_path, "w", encoding="utf-8") as handle:
    json.dump(artifact, handle, indent=2, sort_keys=True)
    handle.write("\n")
if errors:
    for error in errors:
        print(error, file=sys.stderr)
    sys.exit(1)
PY

  local corpus_dir="${out_dir}/sim-regression-${WEEKLY_SIM_CORPUS:-weekly-fixed-v1}"
  step "Named simulation regression corpus (${WEEKLY_SIM_CORPUS:-weekly-fixed-v1}, ${search_seeds} seeds, shard ${search_shard})"
  cargo run -p lash-sim --locked -- run \
    --out "$corpus_dir" \
    --profile "$search_profile" \
    --seeds "$search_seeds" \
    --max-boundaries "$search_max_boundaries" \
    --shard "$search_shard" \
    --mode search \
    --corpus "${WEEKLY_SIM_CORPUS:-weekly-fixed-v1}"
}

run_focused_sqlite_seed_tail_repro() {
  mkdir -p "${out_dir}/sim"
  local repro_dir="${out_dir}/sim/focused-sqlite-seed-tail"
  local repro_artifact="${repro_dir}/focused-sqlite-seed-tail.json"
  if [ "$lane" = "fast" ] && [ "${LASH_RUN_FOCUSED_SQLITE_REPRO_IN_FAST:-0}" != "1" ]; then
    mkdir -p "$repro_dir"
    cat >"$repro_artifact" <<EOF
{
  "schema": "lash.confidence.focused-sqlite-seed-tail-repro.v1",
  "status": "not_run",
  "lane": "${lane}",
  "reason": "focused full-random SQLite seed-tail repro runs in default/broad/full; set LASH_RUN_FOCUSED_SQLITE_REPRO_IN_FAST=1 to include it in fast",
  "exact_command": "scripts/lash-sim-focused-sqlite-repro.sh ${repro_dir}",
  "seeds": [17785827714152183977, 4101155038242989457]
}
EOF
    return
  fi

  step "Focused generated SQLite seed-tail repro"
  scripts/lash-sim-focused-sqlite-repro.sh "$repro_dir"
}

write_provider_transport_exclusion_evidence() {
  step "Provider transport exclusion contract"
  python3 - "${out_dir}/sim/summary.json" "${out_dir}/sim/provider-transport-exclusions.json" <<'PY'
import json
import sys

summary_path, output_path = sys.argv[1:3]
with open(summary_path, "r", encoding="utf-8") as handle:
    summary = json.load(handle)

required_exclusions = {
    "crates/lash-provider-openai/src/codex.rs": "codex websocket transport lane",
    "crates/lash-provider-openai/src/codex/oauth.rs": "auth-flow conformance lane",
    "crates/lash-provider-google/src/oauth.rs": "auth-flow conformance lane",
    "crates/lash-core/src/runtime/session_manager/direct.rs": "runtime direct-effect scenario contracts",
}
required_runtime_providers = {
    "openai-compatible",
    "openai",
    "anthropic",
    "google_oauth",
}

exclusions = summary.get("provider_transport_exclusions") or []
by_path = {entry.get("path"): entry for entry in exclusions}
errors = []
extra_exclusions = sorted(path for path in by_path if path not in required_exclusions)
if extra_exclusions:
    errors.append(
        "unexpected provider transport exclusions require gate review: "
        + ", ".join(extra_exclusions)
    )
for path, lane_fragment in sorted(required_exclusions.items()):
    entry = by_path.get(path)
    if entry is None:
        errors.append(f"missing reviewed provider exclusion for {path}")
        continue
    try:
        with open(path, "r", encoding="utf-8") as handle:
            source = handle.read()
    except OSError as err:
        errors.append(f"{path} could not be read for drift check: {err}")
        source = ""
    if entry.get("status") != "reviewed_non_dst_exclusion":
        errors.append(f"{path} has status {entry.get('status')!r}, expected reviewed_non_dst_exclusion")
    replacement_lane = entry.get("replacement_lane") or ""
    if lane_fragment not in replacement_lane:
        errors.append(f"{path} replacement_lane does not name {lane_fragment!r}")
    if not entry.get("reason"):
        errors.append(f"{path} has no exclusion reason")
    if not entry.get("review_owner"):
        errors.append(f"{path} has no review owner")
    if path.endswith("oauth.rs") and "oauth" not in source.lower():
        errors.append(f"{path} no longer looks like an OAuth surface; update or remove the exclusion")
    if path.endswith("codex.rs") and "codex" not in source.lower():
        errors.append(f"{path} no longer looks like a Codex surface; update or remove the exclusion")
    if path.endswith("direct.rs") and "direct" not in source.lower():
        errors.append(f"{path} no longer looks like a direct runtime surface; update or remove the exclusion")

runtime_matrix = summary.get("generated_runtime_provider_matrix") or []
runtime_providers = {
    entry.get("provider_kind")
    for entry in runtime_matrix
    if (entry.get("runtime_provider_turn_count") or 0) > 0
}
missing_runtime = sorted(required_runtime_providers - runtime_providers)
if missing_runtime:
    errors.append(
        "generated runtime provider matrix missing scripted no-live provider execution for "
        + ", ".join(missing_runtime)
    )

artifact = {
    "schema": "lash.confidence.provider-transport-exclusions.v1",
    "status": "failed" if errors else "passed",
    "semantics": "Codex/OAuth/direct reqwest surfaces are enforced as reviewed non-DST exclusions while generated runtime turns must still execute OpenAI-compatible, direct OpenAI, Anthropic, and Google provider scripts through migrated no-live provider transports.",
    "summary_path": summary_path,
    "required_exclusions": sorted(required_exclusions),
    "required_runtime_providers": sorted(required_runtime_providers),
    "runtime_providers_observed": sorted(provider for provider in runtime_providers if provider),
    "exclusions": exclusions,
    "drift_policy": {
        "unexpected_exclusions": "fail",
        "missing_required_exclusion": "fail",
        "source_surface_mismatch": "fail",
        "replacement_lane_mismatch": "fail",
    },
    "errors": errors,
}
with open(output_path, "w", encoding="utf-8") as handle:
    json.dump(artifact, handle, indent=2, sort_keys=True)
    handle.write("\n")
if errors:
    for error in errors:
        print(error, file=sys.stderr)
    sys.exit(1)
PY
}

write_sim_lane_declarations() {
  mkdir -p "${out_dir}/sim"
  local postgres_status
  if [ "$lane" = "full" ]; then
    if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]; then
      postgres_status="configured_by_env"
    elif command -v docker >/dev/null 2>&1; then
      postgres_status="full_lane_bootstraps_docker"
    else
      postgres_status="full_lane_requires_LASH_POSTGRES_DATABASE_URL_or_docker"
    fi
  elif [ "$lane" = "broad" ]; then
    if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]; then
      postgres_status="broad_lane_configured_by_env"
    elif command -v docker >/dev/null 2>&1; then
      postgres_status="broad_lane_bootstraps_docker"
    else
      postgres_status="broad_lane_skips_postgres_without_LASH_POSTGRES_DATABASE_URL_or_docker"
    fi
  elif [ "$lane" = "default" ]; then
    if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]; then
      postgres_status="current_trace_replay_configured_by_env"
    elif command -v docker >/dev/null 2>&1; then
      postgres_status="current_trace_replay_bootstraps_docker"
    else
      postgres_status="current_trace_replay_requires_LASH_POSTGRES_DATABASE_URL_or_docker"
    fi
  else
    postgres_status="env_gated_full_lane_only"
  fi
  cat >"${out_dir}/sim/env-gated-lanes.json" <<EOF
{
  "schema": "lash.confidence.env-gated-lanes.v1",
  "lane": "${lane}",
  "sqlite_runtime_replay": "included_in_lash_sim_run",
  "minimized_regression_packages": "included_in_lash_sim_run",
  "operational_coverage_oracle": "sim.oracle.operational-coverage.v1",
  "operational_cases": "queueing_inputs,triggers,cancellation,observer_reconnects,provider_failures_mutations,process_wakes,tool_exec,durable_effects,worker_lease_failover,backend_choices,retries,duplicates",
  "scenario_contract_manifests": "included_in_lash_sim_summary",
  "scenario_contract_slices": "included_in_lash_sim_summary_with_generated_shape_transition_kind_and_negative_fixture",
  "sim_search_run": "$(scheduled_existing_artifact_path sim_search_run sim/search.json "$(schedule_lane_fallback_reason)")",
  "focused_sqlite_seed_tail_repro": "$(scheduled_existing_artifact_path focused_sqlite_seed_tail_repro sim/focused-sqlite-seed-tail/focused-sqlite-seed-tail.json not_written)",
  "generated_postgres_dynamic_replay": "$(scheduled_artifact_path generated_postgres_dynamic_replay sim/postgres-generated-rerun/summary.json "$(schedule_lane_fallback_reason)")",
  "model_only_boundary_reviews": "included_in_lash_sim_summary",
  "provider_transport_exclusions": "$(scheduled_artifact_path provider_transport_exclusions sim/provider-transport-exclusions.json not_in_selected_schedule)",
  "backend_contention": "$(scheduled_artifact_path backend_contention sim/backend-contention/backend-contention.json "$(schedule_lane_fallback_reason)")",
  "model_replay_evidence": "$(scheduled_artifact_path model_replay_evidence sim/model-replay/summary.json "$(schedule_lane_fallback_reason)")",
  "postgres_backend_conformance": "${postgres_status}",
  "postgres_trace_replay": "${postgres_status}",
  "postgres_native_effect_history_replay": "native_postgres_runtime_effect_controller",
  "postgres_effect_history_evidence": "Postgres trace replay report includes effect_history_replay.status=native_postgres_runtime_effect_controller and runtime_effect.controller=postgres_runtime_effect_controller for durable/tool/exec runtime boundaries",
  "postgres_env": "LASH_POSTGRES_DATABASE_URL"
}
EOF
}

write_full_lane_prerequisites() {
  mkdir -p "${out_dir}/sim"
  local cargo_llvm_cov cargo_mutants docker_available llvm_tools postgres_available
  local mutation_postgres_available full_feasible
  if command -v cargo-llvm-cov >/dev/null 2>&1; then
    cargo_llvm_cov="available"
  else
    cargo_llvm_cov="missing"
  fi
  if command -v cargo-mutants >/dev/null 2>&1; then
    cargo_mutants="available"
  else
    cargo_mutants="missing"
  fi
  if command -v docker >/dev/null 2>&1; then
    docker_available="available"
  else
    docker_available="missing"
  fi
  if [ -n "${LLVM_COV:-}" ] && [ -n "${LLVM_PROFDATA:-}" ]; then
    llvm_tools="available_by_env"
  elif command -v rustup >/dev/null 2>&1 \
    && rustup component list --installed | grep -Eq '^llvm-tools-preview($|-)'; then
    llvm_tools="available_by_rustup_component"
  elif command -v nix >/dev/null 2>&1; then
    llvm_tools="bootstrap_available_by_nix"
  else
    llvm_tools="missing"
  fi
  if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]; then
    postgres_available="available_by_env"
  elif command -v docker >/dev/null 2>&1; then
    postgres_available="bootstrap_available_by_docker"
  else
    postgres_available="missing"
  fi
  if command -v docker >/dev/null 2>&1; then
    mutation_postgres_available="isolated_ephemeral_docker"
  else
    mutation_postgres_available="missing"
  fi
  if [ "$cargo_llvm_cov" = "available" ] \
    && [ "$cargo_mutants" = "available" ] \
    && [ "$llvm_tools" != "missing" ] \
    && [ "$postgres_available" != "missing" ] \
    && [ "$mutation_postgres_available" != "missing" ]; then
    full_feasible="true"
  else
    full_feasible="false"
  fi
  cat >"${out_dir}/sim/full-lane-prerequisites.json" <<EOF
{
  "schema": "lash.confidence.full-lane-prerequisites.v1",
  "lane": "${lane}",
  "full_lane_feasible_without_bootstrap": ${full_feasible},
  "tools": {
    "cargo_llvm_cov": "${cargo_llvm_cov}",
    "cargo_mutants": "${cargo_mutants}",
    "llvm_tools": "${llvm_tools}",
    "docker": "${docker_available}",
    "postgres": "${postgres_available}",
    "mutation_postgres": "${mutation_postgres_available}"
  },
  "true_full_command": "LASH_CONFIDENCE_OUT_DIR=${out_root} LASH_CONFIDENCE_MUTATION_SCOPE=full scripts/confidence-gate.sh full",
  "bounded_broad_command": "LASH_CONFIDENCE_OUT_DIR=${out_root} LASH_BROAD_SIM_SEEDS=2 LASH_BROAD_SIM_MAX_BOUNDARIES=128 LASH_MUTATION_JOBS=2 LASH_MUTATION_TIMEOUT_SECONDS=300 scripts/confidence-gate.sh broad",
  "bootstrap_true_full_command": "LASH_CONFIDENCE_BOOTSTRAP=1 LASH_CONFIDENCE_OUT_DIR=${out_root} LASH_CONFIDENCE_MUTATION_SCOPE=full scripts/confidence-gate.sh full",
  "postgres_env": "LASH_POSTGRES_DATABASE_URL",
  "postgres_native_effect_history_replay": {
    "status": "native_postgres_runtime_effect_controller",
    "controller": "lash_postgres_store::PostgresRuntimeEffectController",
    "smallest_required_api_change": "none"
  }
}
EOF
}

write_postgres_effect_history_status() {
  mkdir -p "${out_dir}/sim"
  cat >"${out_dir}/sim/postgres-effect-history-status.json" <<EOF
{
  "schema": "lash.confidence.postgres-effect-history-status.v1",
  "lane": "${lane}",
  "status": "native",
  "native_postgres_effect_history_replay": "claimed",
  "controller": "lash_postgres_store::PostgresRuntimeEffectController",
  "store_table": "lash_runtime_effect_replay",
  "semantics": [
    "scope_id plus replay_key primary key",
    "stable envelope hash conflict rejection",
    "lease owner and token fenced finalize",
    "completed and failed outcome replay",
    "sleep due_at_ms preservation"
  ],
  "evidence": [
    "lash-postgres-store env-gated RuntimeEffectController conformance",
    "lash-sim replay-postgres effect_history_replay.status",
    "durable/tool/exec runtime boundary observations with runtime_effect.controller=postgres_runtime_effect_controller"
  ],
  "smallest_required_api_change": "none"
}
EOF
}

run_local_backend_conformance() {
  step "Sqlite backend conformance"
  cargo test -p lash-internal-sqlite-store --locked --test conformance
}

run_backend_contention_evidence() {
  step "Backend contention/fault evidence"
  cargo run -p lash-sim --locked -- backend-contention --out "${out_dir}/sim/backend-contention"
}

run_generated_postgres_dynamic_replay() {
  local database_url="$1"
  local mode="$2"
  step "Generated Postgres dynamic backend rerun"
  local replay_dir="${out_dir}/sim/postgres-generated-rerun"
  local profile="${LASH_POSTGRES_GENERATED_PROFILE:-full-random}"
  local seed="4101155038242989457"
  local max_boundaries="${LASH_POSTGRES_GENERATED_MAX_BOUNDARIES:-128}"
  LASH_POSTGRES_DATABASE_URL="$database_url" \
    cargo run -p lash-sim --locked -- run-postgres \
      --out "$replay_dir" \
      --profile "$profile" \
      --seed "$seed" \
      --max-boundaries "$max_boundaries"
  python3 - "${replay_dir}/summary.json" "$mode" <<'PY'
import json
import sys

path, mode = sys.argv[1:3]
with open(path, "r", encoding="utf-8") as handle:
    summary = json.load(handle)
summary["postgres_mode"] = mode
summary["confidence_lane"] = "generated_dynamic_postgres_backend_rerun"
summary["semantics"] = (
    "same generated workload rerun through the serialized in-memory reference "
    "and real lash-postgres-store backend; this is dynamic generated-driver "
    "equivalence, not fixed-order trace replay"
)
with open(path, "w", encoding="utf-8") as handle:
    json.dump(summary, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
}

run_cross_backend_store_soak() {
  local database_url="$1"
  local cases="${LASH_CROSS_BACKEND_SOAK_CASES:-64}"
  step "Cross-backend durable-store differential soak (${cases} cases)"
  LASH_POSTGRES_DATABASE_URL="$database_url" \
    LASH_REQUIRE_POSTGRES=1 \
    LASH_CROSS_BACKEND_CASES="$cases" \
    cargo test -p lash-sim --test cross_backend_store_differential --locked \
      generated_cross_backend_surface_differential_agrees -- --nocapture --include-ignored
}

write_generated_postgres_dynamic_replay_skipped() {
  mkdir -p "${out_dir}/sim/postgres-generated-rerun"
  cat >"${out_dir}/sim/postgres-generated-rerun/summary.json" <<EOF
{
  "schema": "lash.sim.postgres-generated-rerun-summary.v1",
  "status": "skipped",
  "reason": "Docker and LASH_POSTGRES_DATABASE_URL are unavailable for generated Postgres dynamic backend rerun",
  "confidence_lane": "generated_dynamic_postgres_backend_rerun"
}
EOF
}

run_postgres_schema_gate() {
  local database_url="$1"
  # The DDL artifact hosts vendor and the expectation every open verifies against
  # are two committed files; the drift gate that keeps them agreeing with each other
  # and with a live catalog needs a real database, so it lives here rather than in
  # the database-free lanes. Without it a green confidence gate would not imply
  # `schema.sql` and `schema-shape.txt` still describe the same schema.
  step "Postgres schema artifact drift and structural check"
  LASH_POSTGRES_DATABASE_URL="$database_url" \
    LASH_REQUIRE_POSTGRES=1 \
    cargo test -p lash-internal-postgres-store --locked --lib schema_shape
  LASH_POSTGRES_DATABASE_URL="$database_url" \
    LASH_REQUIRE_POSTGRES=1 \
    cargo test -p lash-internal-postgres-store --locked --test schema_drift
}

run_postgres_conformance() {
  step "Postgres backend conformance"
  if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]; then
    LASH_REQUIRE_POSTGRES=1 cargo test -p lash-internal-postgres-store --locked --test conformance
    run_postgres_schema_gate "$LASH_POSTGRES_DATABASE_URL"
    run_cross_backend_store_soak "$LASH_POSTGRES_DATABASE_URL"
    run_generated_postgres_dynamic_replay "$LASH_POSTGRES_DATABASE_URL" "env"
    if area_selected sim; then
      run_model_replay_suite
    fi
    run_backend_contention_evidence
    mkdir -p "${out_dir}/sim"
    cat >"${out_dir}/sim/postgres-conformance.json" <<EOF
{
  "schema": "lash.confidence.postgres-conformance.v1",
  "status": "passed",
  "mode": "env",
  "env": "LASH_POSTGRES_DATABASE_URL"
}
EOF
    return
  fi

  if ! command -v docker >/dev/null 2>&1; then
    echo "Full confidence requires Docker or LASH_POSTGRES_DATABASE_URL for Postgres conformance." >&2
    exit 127
  fi

  local container port
  container="lash-confidence-postgres-${LASH_GATE_WORKTREE_SLUG}"
  port="${LASH_CONFIDENCE_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 10))}"
  cleanup_postgres() {
    docker rm -f "$container" >/dev/null 2>&1 || true
  }
  trap cleanup_postgres RETURN

  bash scripts/docker-pull-with-retry.sh postgres:16-alpine
  docker run -d --name "$container" \
    --label "$LASH_GATE_LABEL" \
    --network "$LASH_E2E_NETWORK" \
    -e POSTGRES_USER=lash \
    -e POSTGRES_PASSWORD=lash \
    -e POSTGRES_DB=lash \
    -p "127.0.0.1:${port}:5432" \
    postgres:16-alpine >/dev/null

  local deadline=$((SECONDS + 60))
  until docker exec "$container" pg_isready -U lash -d lash >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$container" >&2 || true
      echo "Postgres did not become ready on port ${port}" >&2
      exit 1
    fi
    sleep 1
  done

  LASH_POSTGRES_DATABASE_URL="postgres://lash:lash@127.0.0.1:${port}/lash" \
    LASH_REQUIRE_POSTGRES=1 \
    cargo test -p lash-internal-postgres-store --locked --test conformance
  run_postgres_schema_gate "postgres://lash:lash@127.0.0.1:${port}/lash"
  run_cross_backend_store_soak "postgres://lash:lash@127.0.0.1:${port}/lash"
  run_generated_postgres_dynamic_replay "postgres://lash:lash@127.0.0.1:${port}/lash" "docker"
  if area_selected sim; then
    run_model_replay_suite
  fi
  LASH_POSTGRES_DATABASE_URL="postgres://lash:lash@127.0.0.1:${port}/lash" \
    run_backend_contention_evidence
  mkdir -p "${out_dir}/sim"
  cat >"${out_dir}/sim/postgres-conformance.json" <<EOF
{
  "schema": "lash.confidence.postgres-conformance.v1",
  "status": "passed",
  "mode": "docker",
  "image": "postgres:16-alpine",
  "port": "${port}"
}
EOF
}

write_restate_postgres_workers_e2e_lane_status() {
  if [ "$lane" = "full" ]; then
    return
  fi
  mkdir -p "${out_dir}/sim"
  cat >"${out_dir}/sim/restate-postgres-workers-e2e.json" <<EOF
{
  "schema": "lash.confidence.restate-postgres-workers-e2e.v1",
  "status": "not_run",
  "lane": "${lane}",
  "reason": "distributed Restate/Postgres/MinIO worker e2e is full-lane-only",
  "script": "scripts/restate-postgres-workers-e2e.sh",
  "full_lane_command": "LASH_CONFIDENCE_OUT_DIR=${out_root} LASH_CONFIDENCE_MUTATION_SCOPE=full scripts/confidence-gate.sh full"
}
EOF
}

run_restate_postgres_workers_e2e() {
  if [ "$lane" != "full" ]; then
    return
  fi
  step "Restate/Postgres/MinIO workers e2e"
  local artifact log_dir minio_port exit_code
  artifact="${out_dir}/sim/restate-postgres-workers-e2e.json"
  log_dir="${out_dir}/sim/restate-postgres-workers-e2e"
  minio_port="${LASH_CONFIDENCE_RESTATE_WORKERS_MINIO_PORT:-$((LASH_E2E_PORT_BASE + 40))}"
  mkdir -p "$log_dir"
  set +e
  LASH_E2E_MINIO_PORT="$minio_port" \
    bash scripts/restate-postgres-workers-e2e.sh \
    >"${log_dir}/stdout.log" 2>"${log_dir}/stderr.log"
  exit_code=$?
  set -e
  if [ "$exit_code" -eq 0 ]; then
    cat >"$artifact" <<EOF
{
  "schema": "lash.confidence.restate-postgres-workers-e2e.v1",
  "status": "passed",
  "lane": "full",
  "script": "scripts/restate-postgres-workers-e2e.sh",
  "minio_port": "${minio_port}",
  "stdout": "sim/restate-postgres-workers-e2e/stdout.log",
  "stderr": "sim/restate-postgres-workers-e2e/stderr.log",
  "evidence": "two Restate workers behind proxy with Postgres state, MinIO attachments, host-built worker binaries, and runner-owned end-to-end assertions"
}
EOF
    return
  fi
  cat >"$artifact" <<EOF
{
  "schema": "lash.confidence.restate-postgres-workers-e2e.v1",
  "status": "failed",
  "lane": "full",
  "script": "scripts/restate-postgres-workers-e2e.sh",
  "exit_code": ${exit_code},
  "minio_port": "${minio_port}",
  "stdout": "sim/restate-postgres-workers-e2e/stdout.log",
  "stderr": "sim/restate-postgres-workers-e2e/stderr.log",
  "exact_retry_command": "LASH_CONFIDENCE_OUT_DIR=${out_root} LASH_CONFIDENCE_MUTATION_SCOPE=full scripts/confidence-gate.sh full"
}
EOF
  write_confidence_summary "failed"
  exit "$exit_code"
}

run_broad_postgres_evidence() {
  if [ "$lane" != "broad" ]; then
    return
  fi
  step "Broad Postgres/static replay evidence"
  if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]; then
    LASH_REQUIRE_POSTGRES=1 cargo test -p lash-internal-postgres-store --locked --test conformance
    run_generated_postgres_dynamic_replay "$LASH_POSTGRES_DATABASE_URL" "env"
    if area_selected sim; then
      run_model_replay_suite
    fi
    run_backend_contention_evidence
    mkdir -p "${out_dir}/sim"
    cat >"${out_dir}/sim/postgres-conformance.json" <<EOF
{
  "schema": "lash.confidence.postgres-conformance.v1",
  "status": "passed",
  "mode": "env",
  "env": "LASH_POSTGRES_DATABASE_URL"
}
EOF
    return
  fi

  if ! command -v docker >/dev/null 2>&1; then
    if area_selected sim; then
      run_model_replay_suite
    fi
    write_generated_postgres_dynamic_replay_skipped
    mkdir -p "${out_dir}/sim"
    cat >"${out_dir}/sim/postgres-conformance.json" <<EOF
{
  "schema": "lash.confidence.postgres-conformance.v1",
  "status": "skipped",
  "mode": "unavailable",
  "reason": "Docker and LASH_POSTGRES_DATABASE_URL are unavailable for the broad lane"
}
EOF
    return
  fi

  local container port
  container="lash-confidence-broad-postgres-${LASH_GATE_WORKTREE_SLUG}"
  port="${LASH_CONFIDENCE_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 10))}"
  cleanup_postgres_broad() {
    docker rm -f "$container" >/dev/null 2>&1 || true
  }
  trap cleanup_postgres_broad RETURN

  bash scripts/docker-pull-with-retry.sh postgres:16-alpine
  docker run -d --name "$container" \
    --label "$LASH_GATE_LABEL" \
    --network "$LASH_E2E_NETWORK" \
    -e POSTGRES_USER=lash \
    -e POSTGRES_PASSWORD=lash \
    -e POSTGRES_DB=lash \
    -p "127.0.0.1:${port}:5432" \
    postgres:16-alpine >/dev/null

  local deadline=$((SECONDS + 60))
  until docker exec "$container" pg_isready -U lash -d lash >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$container" >&2 || true
      echo "Postgres did not become ready on port ${port}" >&2
      exit 1
    fi
    sleep 1
  done

  LASH_POSTGRES_DATABASE_URL="postgres://lash:lash@127.0.0.1:${port}/lash" \
    LASH_REQUIRE_POSTGRES=1 \
    cargo test -p lash-internal-postgres-store --locked --test conformance
  run_generated_postgres_dynamic_replay "postgres://lash:lash@127.0.0.1:${port}/lash" "docker"
  if area_selected sim; then
    run_model_replay_suite
  fi
  LASH_POSTGRES_DATABASE_URL="postgres://lash:lash@127.0.0.1:${port}/lash" \
    run_backend_contention_evidence
  mkdir -p "${out_dir}/sim"
  cat >"${out_dir}/sim/postgres-conformance.json" <<EOF
{
  "schema": "lash.confidence.postgres-conformance.v1",
  "status": "passed",
  "mode": "docker",
  "image": "postgres:16-alpine",
  "port": "${port}"
}
EOF
}

run_current_postgres_trace_replay_evidence() {
  if [ "$lane" != "default" ]; then
    return
  fi
  step "Current Postgres trace replay evidence"
  mkdir -p "${out_dir}/sim/postgres-current"
  if [ -n "${LASH_POSTGRES_DATABASE_URL:-}" ]; then
    run_sim_postgres_replay "$LASH_POSTGRES_DATABASE_URL" "env"
    run_backend_contention_evidence
    cat >"${out_dir}/sim/postgres-current/status.json" <<EOF
{
  "schema": "lash.confidence.postgres-current-trace-replay.v1",
  "status": "passed",
  "mode": "env",
  "report": "../postgres-replay/postgres-replay.json",
  "full_lane_status": "not_run_in_default_lane"
}
EOF
    return
  fi
  if ! command -v docker >/dev/null 2>&1; then
    cat >"${out_dir}/sim/postgres-current/status.json" <<EOF
{
  "schema": "lash.confidence.postgres-current-trace-replay.v1",
  "status": "skipped",
  "reason": "Docker and LASH_POSTGRES_DATABASE_URL are unavailable",
  "exact_command": "LASH_POSTGRES_DATABASE_URL=postgres://... LASH_CONFIDENCE_OUT_DIR=${out_root} scripts/confidence-gate.sh default",
  "full_lane_status": "not_run_in_default_lane"
}
EOF
    return
  fi

  local container port
  container="lash-confidence-current-postgres-${LASH_GATE_WORKTREE_SLUG}"
  port="${LASH_CONFIDENCE_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 10))}"
  cleanup_postgres_current() {
    docker rm -f "$container" >/dev/null 2>&1 || true
  }
  trap cleanup_postgres_current RETURN

  bash scripts/docker-pull-with-retry.sh postgres:16-alpine
  docker run -d --name "$container" \
    --label "$LASH_GATE_LABEL" \
    --network "$LASH_E2E_NETWORK" \
    -e POSTGRES_USER=lash \
    -e POSTGRES_PASSWORD=lash \
    -e POSTGRES_DB=lash \
    -p "127.0.0.1:${port}:5432" \
    postgres:16-alpine >/dev/null

  local deadline=$((SECONDS + 60))
  until docker exec "$container" pg_isready -U lash -d lash >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      docker logs "$container" >&2 || true
      echo "Postgres did not become ready on port ${port}" >&2
      exit 1
    fi
    sleep 1
  done

  run_sim_postgres_replay "postgres://lash:lash@127.0.0.1:${port}/lash" "docker"
  LASH_POSTGRES_DATABASE_URL="postgres://lash:lash@127.0.0.1:${port}/lash" \
    run_backend_contention_evidence
  cat >"${out_dir}/sim/postgres-current/status.json" <<EOF
{
  "schema": "lash.confidence.postgres-current-trace-replay.v1",
  "status": "passed",
  "mode": "docker",
  "image": "postgres:16-alpine",
  "port": "${port}",
  "report": "../postgres-replay/postgres-replay.json",
  "full_lane_status": "not_run_in_default_lane"
}
EOF
}

run_sim_postgres_replay() {
  local database_url="$1"
  local mode="$2"
  local trace
  trace="$(
    find "${out_dir}/sim/replays" -name '*.trace.json' -type f 2>/dev/null \
      | sort \
      | head -n 1
  )"
  mkdir -p "${out_dir}/sim/postgres-replay"
  if [ -z "$trace" ]; then
    cat >"${out_dir}/sim/postgres-replay/postgres-replay.json" <<EOF
{
  "schema": "lash.confidence.postgres-trace-replay.v1",
  "status": "skipped",
  "reason": "no generated lash-sim trace was available",
  "mode": "${mode}"
}
EOF
    return
  fi
  LASH_POSTGRES_DATABASE_URL="$database_url" \
    cargo run -p lash-sim --locked -- replay-postgres "$trace" \
      --out "${out_dir}/sim/postgres-replay"
}

run_model_replay_command() {
  local corpus="$1"
  local trace_id="$2"
  local trace="$3"
  local artifact="$4"
  local rows_file="$5"
  mkdir -p "$artifact"
  local exit_code status
  set +e
  cargo run -p lash-sim --locked -- replay "$trace" --out "$artifact" \
    >"${artifact}/stdout.log" 2>"${artifact}/stderr.log"
  exit_code=$?
  set -e
  if [ "$exit_code" -eq 0 ]; then
    status="passed"
  else
    status="failed"
  fi
  printf '{"corpus":"%s","trace_id":"%s","status":"%s","exit_code":%s,"trace_path":"%s","artifact_dir":"%s","stdout":"%s","stderr":"%s"}\n' \
    "$corpus" \
    "$trace_id" \
    "$status" \
    "$exit_code" \
    "$trace" \
    "${artifact#"$out_dir"/}" \
    "${artifact#"$out_dir"/}/stdout.log" \
    "${artifact#"$out_dir"/}/stderr.log" \
    >>"$rows_file"
}

run_model_replay_suite() {
  step "Model replay evidence"
  local replay_dir rows_file
  replay_dir="${out_dir}/sim/model-replay"
  rows_file="${replay_dir}/rows.jsonl"
  rm -rf "$replay_dir"
  mkdir -p "$replay_dir"
  : >"$rows_file"

  local trace trace_id case_dir fixture_name
  while IFS= read -r trace; do
    [ -n "$trace" ] || continue
    trace_id="$(basename "$trace" .trace.json)"
    case_dir="${replay_dir}/generated/${trace_id}"
    run_model_replay_command "generated" "$trace_id" "$trace" "$case_dir" "$rows_file"
  done < <(find "${out_dir}/sim/replays" -name '*.trace.json' -type f 2>/dev/null | sort)

  while IFS= read -r trace; do
    [ -n "$trace" ] || continue
    fixture_name="$(basename "$(dirname "$(dirname "$trace")")")"
    trace_id="${fixture_name}"
    case_dir="${replay_dir}/minimized-failing/${trace_id}"
    run_model_replay_command \
      "minimized_failing_regression" "$trace_id" "$trace" "$case_dir" "$rows_file"
  done < <(find "${out_dir}/sim/failing-fixtures" -path '*/minimized-regression/trace.json' -type f 2>/dev/null | sort)

  while IFS= read -r trace; do
    [ -n "$trace" ] || continue
    fixture_name="$(basename "$(dirname "$trace")")"
    trace_id="${fixture_name}"
    case_dir="${replay_dir}/backend-regression/${trace_id}"
    run_model_replay_command \
      "generated_backend_regression_fixture" "$trace_id" "$trace" "$case_dir" "$rows_file"
  done < <(find "${out_dir}/sim/backend-regression-fixtures" -name 'trace.json' -type f 2>/dev/null | sort)

  python3 - "$rows_file" "${replay_dir}/summary.json" <<'PY'
import json
import sys

rows_path, summary_path = sys.argv[1:3]
rows = []
with open(rows_path, "r", encoding="utf-8") as handle:
    for line in handle:
        line = line.strip()
        if line:
            rows.append(json.loads(line))

by_corpus = {}
for corpus in sorted({row["corpus"] for row in rows}):
    corpus_rows = [row for row in rows if row["corpus"] == corpus]
    trace_ids = sorted({row["trace_id"] for row in corpus_rows})
    by_corpus[corpus] = {
        "trace_count": len(trace_ids),
        "trace_ids": trace_ids,
        "passed": sum(row["status"] == "passed" for row in corpus_rows),
        "failed": sum(row["status"] == "failed" for row in corpus_rows),
    }

failures = [row for row in rows if row["status"] == "failed"]
generated = by_corpus.get("generated", {"trace_count": 0})
generated_backend_regression = by_corpus.get("generated_backend_regression_fixture", {"trace_count": 0})
status = "passed"
if generated["trace_count"] == 0 or generated_backend_regression["trace_count"] == 0 or failures:
    status = "failed"

summary = {
    "schema": "lash.confidence.model-replay-evidence.v1",
    "status": status,
    "semantics": "Generated scheduler traces and generated backend regression fixtures are replayed against the simulation model. Minimized failing-regression traces are model-replayed to prove deterministic oracle preservation. Backend equivalence is not claimed by this artifact; SQLite evidence is recorded by sim/backend-contention/backend-contention.json and sim/focused-sqlite-seed-tail/focused-sqlite-seed-tail.json, while Postgres generated-rerun evidence is recorded by sim/postgres-generated-rerun/summary.json using the single hardcoded seed 4101155038242989457.",
    "row_count": len(rows),
    "corpora": by_corpus,
    "failures": failures,
    "rows_jsonl": "rows.jsonl",
}
with open(summary_path, "w", encoding="utf-8") as handle:
    json.dump(summary, handle, indent=2, sort_keys=True)
    handle.write("\n")
print(status)
PY
  local replay_status
  replay_status="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"])' "${replay_dir}/summary.json")"
  if [ "$replay_status" != "passed" ]; then
    echo "Model replay evidence failed; see ${replay_dir}/summary.json" >&2
    exit 1
  fi
}

run_coverage_blind_spots() {
  step "Coverage blind-spot map"
  local coverage_dir="${out_dir}/coverage"
  mkdir -p "$coverage_dir"
  if [ "$coverage_scope" = "none" ]; then
    cat >"${coverage_dir}/summary.json" <<EOF
{
  "schema": "lash.confidence.coverage-summary.v1",
  "lane": "${lane}",
  "status": "not_run",
  "scope": "none",
  "reason": "LASH_CONFIDENCE_COVERAGE_SCOPE=none requested a bounded replay/backend lane without cargo-llvm-cov",
  "full_lane_command": "LASH_CONFIDENCE_OUT_DIR=${out_root} LASH_CONFIDENCE_MUTATION_SCOPE=full scripts/confidence-gate.sh full"
}
EOF
    return
  fi
  require_tool cargo-llvm-cov cargo-llvm-cov 0.8.7
  require_llvm_tools
  cargo llvm-cov clean --workspace
  local coverage_package_args=()
  local package
  for package in "${selected_packages[@]}"; do
    coverage_package_args+=(-p "$package")
  done
  cargo llvm-cov --locked \
    "${coverage_package_args[@]}" \
    --tests \
    --lcov \
    --output-path "${coverage_dir}/lcov.info"
  cargo llvm-cov report --text --show-missing-lines \
    --output-path "${coverage_dir}/missing-lines.txt"
  cargo llvm-cov report --json --summary-only \
    --output-path "${coverage_dir}/summary.json"
  local critical_package_regex
  critical_package_regex="$(IFS='|'; printf '%s' "${selected_packages[*]}")"
  critical_package_regex="${critical_package_regex//lash-internal-lashlang/lashlang}"
  critical_package_regex="${critical_package_regex//lash-internal-/lash-}"
  awk -v critical_package_regex="$critical_package_regex" '
    /^SF:/ {
      file = substr($0, 4)
      total = 0
      uncovered = 0
      next
    }
    /^DA:/ {
      split(substr($0, 4), fields, ",")
      total += 1
      if (fields[2] == 0) {
        uncovered += 1
      }
      next
    }
    /^end_of_record/ {
      if (file ~ ("/crates/(" critical_package_regex ")/") && uncovered > 0) {
        print uncovered "\t" total "\t" file
      }
    }
  ' "${coverage_dir}/lcov.info" \
    | sort -nr \
    >"${coverage_dir}/critical-uncovered-files.tsv"
  cat >"${coverage_dir}/README.md" <<EOF
# Confidence Coverage Blind Spots

Coverage is an observation artifact, not a pass/fail percentage.

- LCOV: ${coverage_dir}/lcov.info
- Missing-line text report: ${coverage_dir}/missing-lines.txt
- Per-file summary JSON: ${coverage_dir}/summary.json
- Critical uncovered file index: ${coverage_dir}/critical-uncovered-files.tsv

Use these outputs to find unexercised contracts in critical runtime,
Lashlang, protocol, and durable-store code.
EOF
}

run_mutation_smoke() {
  step "Mutation smoke shards (${mutation_jobs} concurrent jobs)"
  require_tool cargo-mutants cargo-mutants 27.1.0
  local shard="${LASH_MUTATION_SMOKE_SHARD:-1/64}"
  local timeout="${LASH_MUTATION_TIMEOUT_SECONDS:-180}"
  for package in "${selected_packages[@]}"; do
    if [ "$package" = "lash-internal-postgres-store" ]; then
      run_postgres_mutants_recorded "$package smoke shard" "${out_dir}/mutants-${package}-smoke" \
        cargo mutants \
        -p "$package" \
        "${area_mutation_file_args[@]}" \
        --cargo-arg=--locked \
        --test-tool cargo \
        --shard "$shard" \
        --timeout "$timeout" \
        --minimum-test-timeout 30 \
        --output "${out_dir}/mutants-${package}-smoke"
    else
      run_mutants_recorded "$package smoke shard" "${out_dir}/mutants-${package}-smoke" \
        cargo mutants \
        -p "$package" \
        "${area_mutation_file_args[@]}" \
        --cargo-arg=--locked \
        --test-tool cargo \
        --shard "$shard" \
        --jobs "$mutation_jobs" \
        --timeout "$timeout" \
        --minimum-test-timeout 30 \
        --output "${out_dir}/mutants-${package}-smoke"
    fi
  done
}

run_area_targeted_mutation_evidence() {
  step "Area-targeted ${area} mutation shards (${mutation_jobs} concurrent jobs)"
  require_tool cargo-mutants cargo-mutants 27.1.0
  local shard="${LASH_AREA_MUTATION_SHARD:-1/64}"
  local timeout="${LASH_MUTATION_TIMEOUT_SECONDS:-180}"
  local package artifact
  for package in "${selected_packages[@]}"; do
    artifact="${out_dir}/mutants-${package}-area-${area}-targeted"
    if [ "$package" = "lash-internal-postgres-store" ]; then
      run_postgres_mutants_recorded "$package area:${area} targeted shard" "$artifact" \
        cargo mutants \
        -p "$package" \
        "${area_mutation_file_args[@]}" \
        --cargo-arg=--locked \
        --test-tool cargo \
        --shard "$shard" \
        --timeout "$timeout" \
        --minimum-test-timeout 30 \
        --output "$artifact"
    else
      run_mutants_recorded "$package area:${area} targeted shard" "$artifact" \
        cargo mutants \
        -p "$package" \
        "${area_mutation_file_args[@]}" \
        --cargo-arg=--locked \
        --test-tool cargo \
        --shard "$shard" \
        --jobs "$mutation_jobs" \
        --timeout "$timeout" \
        --minimum-test-timeout 30 \
        --output "$artifact"
    fi
  done
}

run_lash_core_direct_model_mutation_evidence() {
  step "Lash-core direct/model mutation evidence (${mutation_jobs} concurrent jobs)"
  require_tool cargo-mutants cargo-mutants 27.1.0
  local timeout="${LASH_MUTATION_TIMEOUT_SECONDS:-180}"
  run_mutants_recorded "lash-core direct provider/direct request survivors" "${out_dir}/mutants-lash-core-direct-targeted" \
    cargo mutants \
    -p lash-internal-core \
    --file crates/lash-core/src/direct.rs \
    --re 'DirectRequest::json_schema|DirectLlmClient::provider|DirectLlmClient::provider_mut|DirectLlmClient::complete|build_llm_request|transport_stream_events_for_direct' \
    --baseline skip \
    --jobs "$mutation_jobs" \
    --timeout "$timeout" \
    --minimum-test-timeout 30 \
    --output "${out_dir}/mutants-lash-core-direct-targeted" \
    -- --locked direct
  run_mutants_recorded "lash-core model token-limit survivors" "${out_dir}/mutants-lash-core-model-targeted" \
    cargo mutants \
    -p lash-internal-core \
    --file crates/lash-core/src/model.rs \
    --re 'ModelSpec::with_limits|ModelSpec::with_variant|ModelSpec::from_token_limits|ModelLimits::from_token_limits|ModelSpec::context_window_tokens|nonzero_token_limit|optional_nonzero_token_limit' \
    --baseline skip \
    --jobs "$mutation_jobs" \
    --timeout "$timeout" \
    --minimum-test-timeout 30 \
    --output "${out_dir}/mutants-lash-core-model-targeted" \
    -- --locked model
}

run_lash_sim_runtime_completion_mutation_evidence() {
  step "Lash-sim scheduler/runtime completion mutation evidence (${mutation_jobs} concurrent jobs)"
  require_tool cargo-mutants cargo-mutants 27.1.0
  local timeout="${LASH_MUTATION_TIMEOUT_SECONDS:-180}"
  run_mutants_recorded "lash-sim scheduler runtime completion queue" "${out_dir}/mutants-lash-sim-scheduler-runtime-completion-targeted" \
    cargo mutants \
    -p lash-sim \
    --file crates/lash-sim/src/scheduler.rs \
    --re 'RuntimeCompletionQueue::register|RuntimeCompletionQueue::take_ready|RuntimeCompletionQueue::mark_completed|RuntimeCompletionQueue::registered_len' \
    --baseline skip \
    --jobs "$mutation_jobs" \
    --timeout "$timeout" \
    --minimum-test-timeout 30 \
    --output "${out_dir}/mutants-lash-sim-scheduler-runtime-completion-targeted" \
    -- --locked
  run_mutants_recorded "lash-sim scheduler-owned and mini-oracles" "${out_dir}/mutants-lash-sim-oracles-runtime-completion-targeted" \
    cargo mutants \
    -p lash-sim \
    --file crates/lash-sim/src/oracles.rs \
    --re 'scheduler_owned_runtime_completions|mini_rlm_lashlang_cell_exec_continues|mini_agent_parallel_spawn_join|mini_agent_durable_input_resolution|mini_standard_provider_error_without_checkpoint' \
    --baseline skip \
    --jobs "$mutation_jobs" \
    --timeout "$timeout" \
    --minimum-test-timeout 30 \
    --output "${out_dir}/mutants-lash-sim-oracles-runtime-completion-targeted" \
    -- --locked
  run_mutants_recorded "lash-sim runtime completion readiness" "${out_dir}/mutants-lash-sim-runner-runtime-completion-targeted" \
    cargo mutants \
    -p lash-sim \
    --file crates/lash-sim/src/runner.rs \
    --re 'runtime_completion_ready|register_ready_runtime_completions|RuntimeCompletionState::next_provider_turn_ready|RuntimeCompletionState::provider_completed|RuntimeCompletionState::durable_completed' \
    --baseline skip \
    --jobs "$mutation_jobs" \
    --timeout "$timeout" \
    --minimum-test-timeout 30 \
    --output "${out_dir}/mutants-lash-sim-runner-runtime-completion-targeted" \
    -- --locked
}

run_mutation_full() {
  step "Full mutation suites (${mutation_jobs} concurrent jobs)"
  require_tool cargo-mutants cargo-mutants 27.1.0
  local timeout="${LASH_MUTATION_TIMEOUT_SECONDS:-600}"
  for package in "${selected_packages[@]}"; do
    if [ "$package" = "lash-internal-postgres-store" ]; then
      run_postgres_mutants_recorded "$package full mutation" "${out_dir}/mutants-${package}-full" \
        cargo mutants \
        -p "$package" \
        "${area_mutation_file_args[@]}" \
        --cargo-arg=--locked \
        --test-tool cargo \
        --timeout "$timeout" \
        --minimum-test-timeout 60 \
        --output "${out_dir}/mutants-${package}-full"
    else
      run_mutants_recorded "$package full mutation" "${out_dir}/mutants-${package}-full" \
        cargo mutants \
        -p "$package" \
        "${area_mutation_file_args[@]}" \
        --cargo-arg=--locked \
        --test-tool cargo \
        --jobs "$mutation_jobs" \
        --timeout "$timeout" \
        --minimum-test-timeout 60 \
        --output "${out_dir}/mutants-${package}-full"
    fi
  done
}

mutation_count() {
  local artifact="$1"
  local outcome="$2"
  local file="${artifact}/mutants.out/${outcome}.txt"
  if [ -f "$file" ]; then
    wc -l <"$file" | tr -d ' '
  else
    printf '0'
  fi
}

mutation_artifact_json() {
  local name="$1"
  local artifact="$2"
  local caught missed timeout unviable status status_path exit_code
  caught="$(mutation_count "$artifact" caught)"
  missed="$(mutation_count "$artifact" missed)"
  timeout="$(mutation_count "$artifact" timeout)"
  unviable="$(mutation_count "$artifact" unviable)"
  status_path="${artifact}/confidence-status.json"
  exit_code="null"
  if [ ! -d "${artifact}/mutants.out" ] && [ ! -f "$status_path" ]; then
    status="not_run"
  elif [ "$missed" != "0" ] || [ "$timeout" != "0" ]; then
    status="failed"
  elif [ -f "$status_path" ] && grep -q '"status": "failed"' "$status_path"; then
    status="failed"
    exit_code="$(awk -F': ' '/"exit_code"/ { gsub(/,/, "", $2); print $2; exit }' "$status_path")"
  else
    status="passed"
    if [ -f "$status_path" ]; then
      exit_code="$(awk -F': ' '/"exit_code"/ { gsub(/,/, "", $2); print $2; exit }' "$status_path")"
    fi
  fi
  printf '{"name":"%s","status":"%s","artifact":"%s","command_status":"%s","caught":%s,"missed":%s,"timeout":%s,"unviable":%s,"exit_code":%s}' \
    "$name" \
    "$status" \
    "${artifact#"$out_dir"/}" \
    "$([ -f "$status_path" ] && echo "${artifact#"$out_dir"/}/confidence-status.json" || echo "not_run")" \
    "$caught" \
    "$missed" \
    "$timeout" \
    "$unviable" \
    "${exit_code:-null}"
}

full_mutation_suites_complete() {
  local package
  for package in "${selected_packages[@]}"; do
    if [ ! -f "${out_dir}/mutants-${package}-full/confidence-status.json" ]; then
      return 1
    fi
  done
}

full_mutation_status() {
  if [ "$lane" = "full" ] && [ "$mutation_scope" = "full" ]; then
    if full_mutation_suites_complete; then
      echo "run"
    else
      echo "incomplete_full_mutation_suites"
    fi
    return
  fi
  if [ "$lane" = "full" ]; then
    echo "not_run_by_mutation_scope_${mutation_scope}"
  else
    echo "not_run_in_${lane}_lane"
  fi
}

mutation_evidence_status() {
  if [ "$lane" = "fast" ]; then
    echo "not_in_fast_lane"
    return
  fi
  if [ "$mutation_scope" = "none" ]; then
    echo "not_run_by_scope"
    return
  fi
  if [ "$mutation_commands_run" -eq 0 ]; then
    echo "required_not_run"
    return
  fi
  if [ "$lane" = "full" ] \
    && [ "$mutation_scope" = "full" ] \
    && ! full_mutation_suites_complete; then
    echo "incomplete_full_mutation_suites"
    return
  fi
  if [ "$mutation_failures" -eq 0 ]; then
    echo "passed"
  else
    echo "failed"
  fi
}

finalize_mutation_gate() {
  write_mutation_evidence_summary
  if [ "$mutation_failures" -ne 0 ] || [ "$(mutation_evidence_status)" = "required_not_run" ]; then
    write_confidence_summary "failed"
    return 1
  fi
  return 0
}

coverage_evidence_status() {
  if [ "$lane" = "fast" ]; then
    echo "not_in_fast_lane"
  elif [ "$coverage_scope" = "none" ]; then
    echo "not_run_by_scope"
  elif [ -f "${out_dir}/coverage/summary.json" ]; then
    echo "present"
  else
    echo "required_not_written"
  fi
}

mutation_evidence_path() {
  if [ "$lane" = "fast" ]; then
    echo "not_in_fast_lane"
  elif [ -f "${out_dir}/mutation-evidence.json" ]; then
    echo "mutation-evidence.json"
  else
    echo "not_written"
  fi
}

restate_postgres_workers_e2e_status() {
  local artifact="${out_dir}/sim/restate-postgres-workers-e2e.json"
  if [ ! -f "$artifact" ]; then
    echo "not_written"
  elif grep -q '"status": "passed"' "$artifact"; then
    echo "passed"
  elif grep -q '"status": "failed"' "$artifact"; then
    echo "failed"
  elif grep -q '"status": "not_run"' "$artifact"; then
    echo "not_run"
  else
    echo "present_unknown"
  fi
}

write_mutation_evidence_summary() {
  if [ "$lane" = "fast" ]; then
    return
  fi
  local path="${out_dir}/mutation-evidence.json"
  local evidence_status mutation_semantics
  evidence_status="$(mutation_evidence_status)"
  if [ "$lane" = "full" ] && [ "$area" = "all" ]; then
    mutation_semantics="true full lane requires targeted, smoke, and full critical-package cargo-mutants artifacts; not_run shards are never counted as passed"
  elif [ "$lane" = "full" ]; then
    mutation_semantics="explicit area-scoped full depth requires targeted, smoke, and full cargo-mutants artifacts for the selected packages and source filters; it does not claim global full confidence"
  else
    mutation_semantics="bounded cargo-mutants evidence; not_run shards are explicitly outside the configured mutation scope and are not counted as passed"
  fi
  {
    cat <<EOF
{
  "schema": "lash.confidence.mutation-evidence.v1",
  "lane": "${lane}",
  "status": "${evidence_status}",
  "scope": "${mutation_scope}",
  "area": "${area}",
  "semantics": "${mutation_semantics}",
  "targeted_regressions": [
    $(mutation_artifact_json "lash-core direct provider/direct request survivors" "${out_dir}/mutants-lash-core-direct-targeted"),
    $(mutation_artifact_json "lash-core model token-limit survivors" "${out_dir}/mutants-lash-core-model-targeted"),
    $(mutation_artifact_json "lash-sim scheduler runtime completion queue" "${out_dir}/mutants-lash-sim-scheduler-runtime-completion-targeted"),
    $(mutation_artifact_json "lash-sim scheduler-owned and mini-oracles" "${out_dir}/mutants-lash-sim-oracles-runtime-completion-targeted"),
    $(mutation_artifact_json "lash-sim runtime completion readiness" "${out_dir}/mutants-lash-sim-runner-runtime-completion-targeted")
  ],
  "area_targeted_shards": [
EOF
    local package
    first=1
    if [ "$area" != "all" ]; then
      for package in "${selected_packages[@]}"; do
        if [ "$first" = "1" ]; then
          first=0
        else
          printf ',\n'
        fi
        printf '    '
        mutation_artifact_json "$package area:${area} targeted shard" "${out_dir}/mutants-${package}-area-${area}-targeted"
      done
    fi
    cat <<EOF

  ],
  "critical_package_smoke_shards": [
EOF
    first=1
    for package in "${selected_packages[@]}"; do
      if [ "$first" = "1" ]; then
        first=0
      else
        printf ',\n'
      fi
      printf '    '
      mutation_artifact_json "$package" "${out_dir}/mutants-${package}-smoke"
    done
    cat <<EOF

  ],
  "full_mutation_suites": [
EOF
    first=1
    for package in "${selected_packages[@]}"; do
      if [ "$first" = "1" ]; then
        first=0
      else
        printf ',\n'
      fi
      printf '    '
      mutation_artifact_json "$package full mutation" "${out_dir}/mutants-${package}-full"
    done
    cat <<EOF

  ],
  "smoke_shard": "${LASH_MUTATION_SMOKE_SHARD:-1/64}",
  "critical_package_smoke_status": "$([[ "$mutation_scope" = "smoke" || "$mutation_scope" = "full" ]] && echo "run" || echo "not_run_by_mutation_scope")",
  "full_mutation_status": "$(full_mutation_status)",
  "true_full_command": "LASH_CONFIDENCE_OUT_DIR=${out_root} LASH_CONFIDENCE_MUTATION_SCOPE=full scripts/confidence-gate.sh full",
  "bounded_broad_command": "LASH_CONFIDENCE_OUT_DIR=${out_root} LASH_BROAD_SIM_SEEDS=2 LASH_BROAD_SIM_MAX_BOUNDARIES=128 LASH_MUTATION_JOBS=2 LASH_MUTATION_TIMEOUT_SECONDS=300 scripts/confidence-gate.sh broad"
}
EOF
  } >"$path"
}

confidence_class() {
  case "$lane:$area" in
    broad:all) echo "bounded_broad" ;;
    broad:*) echo "area_scoped_bounded_broad" ;;
    full:all) echo "true_full" ;;
    full:*) echo "area_scoped_full" ;;
    default:all) echo "default_targeted" ;;
    default:*) echo "area_scoped_default_targeted" ;;
    fast:all) echo "fast" ;;
    fast:*) echo "area_scoped_fast" ;;
  esac
}

write_confidence_summary() {
  local status="${1:-passed}"
  cat >"${out_dir}/confidence-summary.json" <<EOF
{
  "schema": "lash.confidence.summary.v1",
  "lane": "${lane}",
  "selector": "${requested_selector}",
  "area": "${area}",
  "status": "${status}",
  "sim_summary": "$(scheduled_artifact_path sim_summary sim/summary.json not_in_selected_schedule)",
  "env_gated_lanes": "$(scheduled_artifact_path env_gated_lanes sim/env-gated-lanes.json not_in_selected_schedule)",
  "full_lane_prerequisites": "$(scheduled_artifact_path full_lane_prerequisites sim/full-lane-prerequisites.json not_in_selected_schedule)",
  "failing_minimizer_fixtures": "$(scheduled_artifact_path failing_minimizer_fixtures sim/failing-minimizer-fixtures.json not_in_selected_schedule)",
  "confidence_class": "$(confidence_class)",
  "global_full_confidence_claim": "$([ "$lane" = "full" ] && [ "$area" = "all" ] && echo "true" || echo "false")",
  "coverage_summary": "$(scheduled_existing_artifact_path coverage_summary coverage/summary.json not_run)",
  "coverage_scope": "${coverage_scope}",
  "coverage_evidence_status": "$(coverage_evidence_status)",
  "sim_search_run": "$(scheduled_existing_artifact_path sim_search_run sim/search.json not_run)",
  "focused_sqlite_seed_tail_repro": "$(scheduled_existing_artifact_path focused_sqlite_seed_tail_repro sim/focused-sqlite-seed-tail/focused-sqlite-seed-tail.json not_run)",
  "mutation_evidence": "$(mutation_evidence_path)",
  "mutation_evidence_status": "$(mutation_evidence_status)",
  "mutation_scope": "${mutation_scope}",
  "full_mutation_status": "$(full_mutation_status)",
  "postgres_backend_conformance": "$(if schedule_has_artifact generated_postgres_dynamic_replay; then echo "included_or_explicitly_skipped_in_postgres_conformance_artifact"; else echo "not_in_selected_lane_or_area"; fi)",
  "postgres_current_trace_replay": "$(scheduled_artifact_path postgres_current_trace_replay sim/postgres-current/status.json not_in_selected_lane_or_area)",
  "postgres_current_trace_replay_report": "$(scheduled_existing_artifact_path postgres_current_trace_replay_report sim/postgres-replay/postgres-replay.json not_run)",
  "generated_postgres_dynamic_replay": "$(scheduled_existing_artifact_path generated_postgres_dynamic_replay sim/postgres-generated-rerun/summary.json not_run)",
  "backend_contention": "$(scheduled_existing_artifact_path backend_contention sim/backend-contention/backend-contention.json not_run)",
  "model_replay_evidence": "$(scheduled_existing_artifact_path model_replay_evidence sim/model-replay/summary.json not_run)",
  "restate_postgres_workers_e2e": "$(scheduled_existing_artifact_path restate_postgres_workers_e2e sim/restate-postgres-workers-e2e.json not_written)",
  "provider_transport_exclusions": "$(scheduled_existing_artifact_path provider_transport_exclusions sim/provider-transport-exclusions.json not_written)",
  "postgres_native_effect_history_replay": "native_postgres_runtime_effect_controller",
  "postgres_effect_history_status": "$(scheduled_existing_artifact_path postgres_effect_history_status sim/postgres-effect-history-status.json not_written)",
  "artifact_contract": {
    "schema": "lash.confidence.summary-artifact-contract.v1",
    "full_lane": {
      "confidence_class": "$(confidence_class)",
      "selected_area": "${area}",
      "global_full_confidence_claim": "$([ "$area" = "all" ] && echo "true" || echo "false")",
      "required_coverage_scope": "run",
      "effective_coverage_scope": "${coverage_scope}",
      "coverage_evidence_status": "$(coverage_evidence_status)",
      "required_mutation_scope": "full",
      "effective_mutation_scope": "${mutation_scope}",
      "mutation_evidence": "$(mutation_evidence_path)",
      "mutation_evidence_status": "$(mutation_evidence_status)",
      "full_mutation_status": "$(full_mutation_status)",
      "required_restate_postgres_workers_e2e": "$(scheduled_artifact_path restate_postgres_workers_e2e sim/restate-postgres-workers-e2e.json not_in_selected_area)",
      "restate_postgres_workers_e2e_status": "$(restate_postgres_workers_e2e_status)"
    },
    "bounded_broad_confidence": {
      "confidence_class": "bounded_broad",
      "workflow": "Confidence",
      "lane": "broad",
      "trigger": "workflow_dispatch_or_schedule",
      "artifact_name": "confidence-artifacts-attempt-${GITHUB_RUN_ATTEMPT:-local}",
      "coverage_scope": "${coverage_scope}",
      "coverage_evidence_status": "$(coverage_evidence_status)",
      "mutation_scope": "${mutation_scope}",
      "mutation_evidence_status": "$(mutation_evidence_status)",
      "full_confidence_claim": "false"
    }
  },
  "mutation_testing": "$(if [ "$area" != "all" ]; then echo "area_${area}_configured_${mutation_scope}_scope_explicitly_scoped_mutation"; else case "$lane" in fast) echo "not_in_fast_lane" ;; default) echo "configured_${mutation_scope}_scope_lash_core_direct_model_and_lash_sim_scheduler_oracle_targets" ;; broad) echo "bounded_broad_configured_${mutation_scope}_scope_targeted_regressions_without_full_mutation_claim" ;; full) echo "true_full_configured_full_scope_targeted_smoke_and_full_mutation" ;; esac; fi)",
  "true_full_command": "LASH_CONFIDENCE_OUT_DIR=${out_root} LASH_CONFIDENCE_MUTATION_SCOPE=full scripts/confidence-gate.sh full",
  "bounded_broad_command": "LASH_CONFIDENCE_OUT_DIR=${out_root} scripts/confidence-gate.sh broad",
  "artifacts_root": "${out_dir}"
}
EOF
}

write_fast_shard_summary() {
  local shard="$1"
  mkdir -p "$out_dir"
  cat >"${out_dir}/confidence-summary.json" <<EOF
{
  "schema": "lash.confidence.fast-shard-summary.v1",
  "lane": "fast",
  "shard": "${shard}",
  "status": "passed",
  "duration_seconds": $((SECONDS - script_started_at)),
  "sqlite_substrate_faults": "$(scheduled_existing_artifact_path sqlite_substrate_faults sim/sqlite-substrate-faults/sqlite-faults.json "not_in_${shard}_shard")",
  "artifacts_root": "${out_dir}"
}
EOF
}

write_fast_matrix_summary() {
  mkdir -p "$out_dir"
  python3 - "$out_dir" "${fast_shards[@]}" <<'PY'
import json
import pathlib
import sys

out_dir = pathlib.Path(sys.argv[1])
shards = sys.argv[2:]
errors: list[str] = []
summaries: dict[str, dict[str, object]] = {}

for shard in shards:
    path = out_dir / shard / "confidence-summary.json"
    if not path.exists():
        errors.append(f"missing shard summary: {path}")
        continue
    try:
        summary = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001 - artifact validation should report exact file.
        errors.append(f"invalid shard summary {path}: {exc}")
        continue
    summaries[shard] = summary
    if summary.get("status") != "passed":
        errors.append(f"shard {shard} status is {summary.get('status')!r}")
    if summary.get("lane") != "fast":
        errors.append(f"shard {shard} has lane {summary.get('lane')!r}")
    if summary.get("shard") != shard:
        errors.append(f"shard {shard} summary identifies shard {summary.get('shard')!r}")

status = "failed" if errors else "passed"
artifact = {
    "schema": "lash.confidence.summary.v1",
    "lane": "fast",
    "status": status,
    "confidence_class": "fast",
    "sharded": True,
    "required_shards": shards,
    "shards": {
        shard: {
            "summary": f"{shard}/confidence-summary.json",
            "status": summaries.get(shard, {}).get("status", "missing"),
            "duration_seconds": summaries.get(shard, {}).get("duration_seconds"),
        }
        for shard in shards
    },
    "sim_summary": "sim-generated/sim/summary.json",
    "env_gated_lanes": "sim-generated/sim/env-gated-lanes.json",
    "full_lane_prerequisites": "sim-generated/sim/full-lane-prerequisites.json",
    "failing_minimizer_fixtures": "minimizer-fixtures/sim/failing-minimizer-fixtures.json",
    "provider_transport_exclusions": "sim-generated/sim/provider-transport-exclusions.json",
    "mutation_testing": "not_in_fast_lane",
    "artifacts_root": str(out_dir),
    "errors": errors,
}
summary_path = out_dir / "confidence-summary.json"
summary_path.write_text(json.dumps(artifact, indent=2, sort_keys=True) + "\n", encoding="utf-8")
if errors:
    for error in errors:
        print(error, file=sys.stderr)
    sys.exit(1)
PY
}

run_fast_shard() {
  case "$fast_shard" in
    scenario-harnesses)
      run_scenario_harnesses
      write_fast_shard_summary "$fast_shard"
      ;;
    fault-matrix)
      run_state_machine_and_fault_matrix
      write_fast_shard_summary "$fast_shard"
      ;;
    sim-unit-perf-guards)
      run_sim_unit_suite
      write_fast_shard_summary "$fast_shard"
      ;;
    sim-generated)
      run_sim_generated_lane
      if [ "$area" = "all" ] || area_selected store; then
        run_focused_sqlite_seed_tail_repro
      fi
      write_provider_transport_exclusion_evidence
      write_sim_lane_declarations
      write_full_lane_prerequisites
      write_postgres_effect_history_status
      write_restate_postgres_workers_e2e_lane_status
      write_fast_shard_summary "$fast_shard"
      ;;
    minimizer-fixtures)
      run_minimizer_fixture_suite
      write_fast_shard_summary "$fast_shard"
      ;;
    summary)
      write_fast_matrix_summary
      ;;
    *)
      echo "unknown fast shard: ${fast_shard}" >&2
      exit 2
      ;;
  esac
}

run_fast_aggregate() {
  local shard
  for shard in "${fast_shards[@]}"; do
    LASH_CONFIDENCE_OUT_DIR="$out_root" "$0" "fast:${shard}"
  done
  LASH_CONFIDENCE_OUT_DIR="$out_root" "$0" fast:summary
}

print_plan() {
  printf 'Confidence selector: %s\n' "$requested_selector"
  printf 'Depth: %s\n' "$lane"
  printf 'Area: %s\n' "$area"
  printf 'Mutation scope: %s\n' "$mutation_scope"
  printf 'Coverage scope: %s\n' "$coverage_scope"
  printf 'Artifacts: %s\n' "$out_dir"
  printf 'Would run:\n'

  local selector row row_selector row_area suite description artifacts
  selector="$(schedule_selector)"
  for row in "${confidence_schedule_table[@]}"; do
    IFS='|' read -r row_selector row_area suite description artifacts <<<"$row"
    [ "$row_selector" = "$selector" ] || continue
    schedule_row_matches_area "$row_area" || continue
    [ "$suite" = "metadata" ] && continue
    if [ "$suite" = "sim-search" ]; then
      description="deterministic simulation search shard ${sim_search_shard} at full budgets"
    fi
    printf '  %-11s %s\n' "$row_area" "$description"
  done

  [ -n "$sim_search_shard" ] && return

  if [ "$lane" != "fast" ]; then
  if [ "$coverage_scope" = "run" ]; then
    printf '  coverage    packages: %s\n' "${selected_packages[*]}"
  else
    printf '  coverage    record explicit not_run (scope=none)\n'
  fi
  if [ "$mutation_scope" != "none" ]; then
    if [ "$area" = "all" ]; then
      printf '  mutation    existing %s mutation evidence\n' "$mutation_scope"
    else
      printf '  mutation    area:%s targeted shard for: %s\n' "$area" "${selected_packages[*]}"
      if [ "${#area_mutation_file_args[@]}" -gt 0 ]; then
        printf '  mutation    source filters: %s\n' "${area_mutation_file_args[*]}"
      fi
    fi
  fi
  if [ "$lane" = "full" ]; then
    printf '  mutation    full suites for: %s\n' "${selected_packages[*]}"
  fi
  fi
}

if [ "$dry_run" -eq 1 ]; then
  print_plan
  exit 0
fi

bootstrap_tools

if [ -n "$sim_search_shard" ]; then
  LASH_SIM_SHARD="$sim_search_shard" run_sim_search_lane
  assert_no_panics_in_artifacts
  step "Confidence gate 'sim-search:${sim_search_shard}' passed"
  printf 'Artifacts: %s\n' "$out_dir"
  exit 0
fi

if [ "$lane" = "fast" ]; then
  if [ "$area" != "all" ] && [ "$fast_shard" = "all" ]; then
    run_scenario_harnesses
    run_state_machine_and_fault_matrix
    if area_selected sim; then
      run_sim_provider_scripts
    fi
    write_confidence_summary "passed"
    assert_no_panics_in_artifacts
    step "Confidence gate '${requested_selector}' passed"
    printf 'Artifacts: %s\n' "$out_dir"
    exit 0
  fi
  case "$fast_shard" in
    all)
      run_fast_aggregate
      ;;
    summary)
      run_fast_shard
      ;;
    *)
      run_fast_shard
      ;;
  esac
  assert_no_panics_in_artifacts
  if [ "$fast_shard" = "summary" ] || [ "$fast_shard" = "all" ]; then
    step "Confidence gate 'fast' passed"
    printf 'Artifacts: %s\n' "${out_root}/fast"
  else
    step "Confidence gate 'fast:${fast_shard}' passed"
    printf 'Artifacts: %s\n' "$out_dir"
  fi
  exit 0
fi

run_scenario_harnesses
run_state_machine_and_fault_matrix
if area_selected sim; then
  run_sim_provider_scripts
fi
if area_selected store; then
  run_focused_sqlite_seed_tail_repro
fi
if area_selected sim; then
  write_provider_transport_exclusion_evidence
fi
write_sim_lane_declarations
write_full_lane_prerequisites
write_postgres_effect_history_status
write_restate_postgres_workers_e2e_lane_status

if [ "$lane" = "default" ] || [ "$lane" = "broad" ] || [ "$lane" = "full" ]; then
  if area_selected store; then
    run_local_backend_conformance
    run_backend_contention_evidence
    run_current_postgres_trace_replay_evidence
  fi
  run_coverage_blind_spots
  case "$mutation_scope" in
    targeted|smoke|full)
      if [ "$area" = "all" ]; then
        run_lash_core_direct_model_mutation_evidence
        run_lash_sim_runtime_completion_mutation_evidence
      else
        run_area_targeted_mutation_evidence
      fi
      ;;
    none) ;;
    *)
      echo "Unknown LASH_CONFIDENCE_MUTATION_SCOPE=${mutation_scope}; expected none, targeted, smoke, or full" >&2
      exit 2
      ;;
  esac
  if [ "$mutation_scope" = "smoke" ] || [ "$mutation_scope" = "full" ]; then
    run_mutation_smoke
  fi
fi

if [ "$lane" = "broad" ]; then
  if area_selected store; then
    run_broad_postgres_evidence
  fi
fi

if [ "$lane" = "full" ]; then
  if area_selected store; then
    run_postgres_conformance
  fi
  if area_selected process; then
    run_restate_postgres_workers_e2e
  fi
  run_mutation_full
fi

if [ "$lane" = "default" ] || [ "$lane" = "broad" ] || [ "$lane" = "full" ]; then
  if ! finalize_mutation_gate; then
    exit 1
  fi
fi

assert_no_panics_in_artifacts
write_confidence_summary "passed"

step "Confidence gate '${requested_selector}' passed"
printf 'Artifacts: %s\n' "$out_dir"
