#!/usr/bin/env bash
# Run one service-backed store suite, either from the shared Buck2 cache or
# from a Cargo compile.
#
# Trusted events (`BUCK2_TRUSTED=true`) build the test binaries from the shared
# remote cache and execute them on this runner against the service the job
# stood up. Untrusted events -- fork and Dependabot pull requests -- never
# receive cache credentials, so they run exactly the Cargo commands that
# predate this cutover.
#
# Two properties hold for every Buck2 invocation here and must keep holding:
#
#   * PostgreSQL version and connection settings reach the test only through
#     `--test_env`, which is part of the test spawn and of nothing else. Every
#     compile action key is therefore identical across the PG 14/16/18 matrix
#     legs, and the three jobs share one set of compiled outputs.
#   * `--no-test-cache` prevents service-dependent verdicts from entering the
#     shared cache while leaving compilation cacheable.
#   * `--local-test-execution` keeps test processes on the runner whose
#     loopback hosts the database or bucket. Compilation still uses the pool.
#
# Usage: scripts/ci/store-tests.sh <suite>
#        scripts/ci/store-tests.sh --labels <suite>
#
# `--labels` prints the Buck2 labels the suite executes, one per line, and runs
# nothing. `scripts/ci/store-build.sh` builds them before a service starts.
set -euo pipefail

labels_only=false
if [ "${1:-}" = --labels ]; then
  labels_only=true
  shift
fi
suite="${1:?usage: store-tests.sh [--labels] <suite>}"
trusted=true
if [ "${labels_only}" = false ]; then
  trusted="${BUCK2_TRUSTED:?BUCK2_TRUSTED must be 'true' or 'false'}"
fi
case "${trusted}" in
  true | false) ;;
  *)
    echo "invalid Buck2 trust decision: ${trusted}" >&2
    exit 1
    ;;
esac

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${repo_root}"
selection_python="${LIBTEST_SELECTION_PYTHON:-python3}"

buck2_test_count=0
buck2_report_root=""
buck2_test() {
  buck2_test_count=$((buck2_test_count + 1))
  if [[ -z "$buck2_report_root" ]]; then
    local report_base="${RUNNER_TEMP:-.buck2}/store-test-results"
    local report_name="${suite//[^A-Za-z0-9_.-]/_}"
    mkdir -p "$report_base"
    buck2_report_root="$(mktemp -d "$report_base/${report_name}.XXXXXX")"
  fi
  local test_env=()
  local name
  for name in \
    LASH_POSTGRES_DATABASE_URL \
    LASH_REQUIRE_S3 LASH_S3_ENDPOINT LASH_S3_REGION LASH_S3_BUCKET \
    LASH_S3_ACCESS_KEY LASH_S3_SECRET_KEY LASH_CROSS_BACKEND_CASES; do
    if [[ -v "$name" ]]; then
      test_env+=(--test_env "$name")
    fi
  done
  "${HERMETIC_BUILD:-scripts/hermetic-build.sh}" test \
    --jobs "${LASH_POSTGRES_SLOT_COUNT:-32}" \
    --local-test-execution \
    --no-test-cache \
    --test_timeout 1200 \
    --test_output=all \
    --build-report "$buck2_report_root/build-${buck2_test_count}.json" \
    --test-report "$buck2_report_root/test-${buck2_test_count}.json" \
    --test-output-dir "$buck2_report_root/results-${buck2_test_count}" \
    --event-log "$buck2_report_root/events-${buck2_test_count}.json-lines" \
    "${test_env[@]}" \
    "$@"
  "$selection_python" tools/buck2/libtest_selection.py buck2 \
    "$buck2_report_root/test-${buck2_test_count}.json"
}

cargo_test() {
  local log code=0
  log=$(mktemp)
  "$@" 2>&1 | tee "$log" || code=$?
  if ((code == 0)); then
    "$selection_python" tools/buck2/libtest_selection.py cargo "$log" "$@" || code=$?
  fi
  rm -f "$log"
  return "$code"
}

labels() {
  # Generated inventory: a new service-gated binary joins its service job
  # without a hand edit here. A second argument selects one build of the
  # service's binaries: `default`, or the Cargo feature a feature-lane variant
  # is built with. Each PostgreSQL build has a suite and a CI job of its own.
  python3 - "$@" <<'PY'
import json
import sys
with open("tools/buck2/target-inventory.json", encoding="utf-8") as source:
    inventory = json.load(source)
selected = inventory["service_test_targets"][sys.argv[1]]
if len(sys.argv) > 2:
    features = {unit["label"]: unit["features"] for unit in inventory["feature_lane_units"]}
    if sys.argv[2] == "default":
        selected = [label for label in selected if label not in features]
    else:
        selected = [label for label in selected if sys.argv[2] in features.get(label, ())]
    if not selected:
        raise SystemExit(f"no {sys.argv[1]} service test is built as {sys.argv[2]}")
print(*selected)
PY
}

# The labels the shaped suites below name outside the generated inventory.
readonly restate_ingress_label=//crates/lash-restate:lash-restate__unit_test
readonly catalog_shape_label=//crates/lash-postgres-store:lash-postgres-store__unit_test
readonly catalog_drift_label=//crates/lash-postgres-store:schema_drift__test

# The sharded binaries' shards and the other binaries run in parallel
# (FIG-3572), each under the generated slot wrapper, which gives every test
# action a database of its own out of the LASH_POSTGRES_SLOT_COUNT slots
# `with-service.sh` created; `--jobs` never runs more tests than there are
# slots.
postgres_slot_test() {
  : "${LASH_POSTGRES_SLOT_COUNT:?with-service.sh sets LASH_POSTGRES_SLOT_COUNT}"
  if [ -z "${LASH_POSTGRES_SLOT_DIR:-}" ]; then
    LASH_POSTGRES_SLOT_DIR="$(mktemp -d)"
    export LASH_POSTGRES_SLOT_DIR
  fi
  buck2_test \
    --test_env=LASH_POSTGRES_SLOT_DIR \
    --test_env=LASH_POSTGRES_SLOT_COUNT \
    "$@"
}


# One test selection per uniform suite, rendered into both dialects below.
# Fields are
#
#   comma-separated Buck2 labels|comma-separated test filters|cargo package|cargo target|cargo runner|flags
#
# `skip=<filter>` excludes an explicitly unresolved ignored law from a
# cargo-test suite that otherwise includes its service-only ignored tests.
# Each filter runs separately in either dialect. An empty filter runs the
# whole target.
#
# `flags` is a comma list of intents -- include-ignored, ignored-only,
# single-threaded, nocapture -- that each renderer spells in its own dialect,
# so the translation table that used to be a comment above `pg-cross-backend`
# is code. `runner` is the Cargo side's test driver, kept per suite because
# `--profile ci` and nextest-vs-libtest are execution settings, not test
# selection.
#
# A multi-label row is how a suite selects the same cases from several
# binaries: Buck2 runs each label; Cargo runs the filter once against
# `target`, which must therefore name the union of the labels' test targets
# (an empty `target` selects the whole package's).
#
# Three suites are deliberately absent and stay explicit arms below:
# `pg-catalog-compatibility` runs different targets; `pg-store` and
# `s3-store` take a generated label file rather than one label. Forcing a shape
# variation into the table for those buys nothing.
declare -A uniform_store_suites=(
  # The facade's PostgreSQL-only laws: every `#[ignore]`d test in
  # //crates/lash that a pg16 container alone satisfies. The `postgres` name
  # filter derives them -- scripts/check_postgres_gate_coverage.py fails on a
  # PostgreSQL-gated law whose name or binary escapes it -- and the skips name
  # the laws that also need a second service, which their own suites own.
  [pg-facade-laws]="//crates/lash:lash__unit_test,//crates/lash:facade_host_wrappers__test,//crates/lash:integration__test,//crates/lash:replay_after_advance__test,//crates/lash:seam_proof_dialect__test|postgres|lash-runtime|--lib --bins --test facade_host_wrappers --test integration --test replay_after_advance --test seam_proof_dialect --features restate,rlm,sqlite,testing,typescript|cargo-test|ignored-only,nocapture,skip=postgres_live_restate,skip=live_postgres,skip=native_restate,skip=catalog_storm_native,skip=mcp_law_turn_failures_postgres_live"
  [pg-rlm-frame-open]="//crates/lash-protocol-rlm:frame_open_redrive__test|restate_double_postgres::|lash-internal-protocol-rlm|--test frame_open_redrive|cargo-test|include-ignored,nocapture"
  [pg-rlm-tool-call-limit]="//crates/lash-protocol-rlm:tool_batch_parallelism__test|restate_double_postgres::|lash-internal-protocol-rlm|--test tool_batch_parallelism|cargo-test|include-ignored,nocapture"
  [pg-artifact-referrers]="//crates/lash:artifact_referrers_evidence__test|::postgres|lash-runtime|--test artifact_referrers_evidence --features rlm,restate,sqlite,testing|cargo-test|include-ignored,nocapture"
  [pg-attachment-referrers]="//crates/lash:attachment_referrers_evidence__test|::postgres|lash-runtime|--test attachment_referrers_evidence --features rlm,restate,sqlite,testing|cargo-test|include-ignored,nocapture"
  [pg-model-keys]="//crates/lash:llm_profiles__test||lash-runtime|--test llm_profiles --features restate,sqlite,testing|cargo-test|include-ignored,nocapture"
  [pg-pool-wait]="//crates/lash-perf:lash-perf__unit_test|postgres_pool_checkout_wait_is_recorded_for_runtime_store_reads|lash-perf||nextest|include-ignored"
  [pg-sim-backend-faults]="//crates/lash-sim:lash-sim__unit_test|postgres_backend_fault|lash-sim|--lib|nextest-ci|include-ignored"
  [pg-cross-backend]="//crates/lash-sim:cross_backend_store_differential__test||lash-sim|--test cross_backend_store_differential|nextest-ci|include-ignored,single-threaded,nocapture"
  [s3-attachment-differential]="//crates/lash-sim:cross_backend_store_differential__test|attachment_blob_store_differential_agrees|lash-sim|--test cross_backend_store_differential|cargo-test|include-ignored,nocapture"
)

suite_has_flag() {
  case ",$1," in
    *",$2,"*) return 0 ;;
    *) return 1 ;;
  esac
}

# The Buck2 half: a libtest name filter and the libtest switches, each passed
# through `--test_arg`, plus the output switch `--nocapture` needs to be
# visible in the log.
render_buck2_suite() {
  local label="$1" filter="$2" flags="$3"
  local args=()
  [ -n "$filter" ] && args+=("--test_arg=${filter}")
  suite_has_flag "$flags" include-ignored && args+=(--test_arg=--include-ignored)
  suite_has_flag "$flags" ignored-only && args+=(--test_arg=--ignored)
  suite_has_flag "$flags" single-threaded && args+=(--test_arg=--test-threads=1)
  suite_has_flag "$flags" nocapture && args+=(--test_arg=--nocapture)
  local flag
  local -a selections
  IFS=, read -r -a selections <<< "$flags"
  for flag in "${selections[@]}"; do
    if [[ "$flag" == skip=* ]]; then
      args+=(--test_arg=--skip "--test_arg=${flag#skip=}")
    fi
  done
  buck2_test "${args[@]}" "$label"
}

# The Cargo half: the same selection in nextest's or libtest's spelling.
render_cargo_suite() {
  local filter="$1" package="$2" target="$3" runner="$4" flags="$5"
  local cmd=()
  case "$runner" in
    nextest) cmd=(cargo nextest run) ;;
    nextest-ci) cmd=(cargo nextest run --profile ci) ;;
    cargo-test) cmd=(cargo test) ;;
    *)
      echo "unknown cargo runner: ${runner}" >&2
      exit 1
      ;;
  esac
  cmd+=(-p "$package")
  # Deliberate word splitting: the target selection is this table's own data.
  # shellcheck disable=SC2206
  [ -n "$target" ] && cmd+=($target)
  cmd+=(--locked)
  if [ "$runner" = cargo-test ]; then
    [ -n "$filter" ] && cmd+=("$filter")
    local libtest=()
    suite_has_flag "$flags" nocapture && libtest+=(--nocapture)
    suite_has_flag "$flags" include-ignored && libtest+=(--include-ignored)
    suite_has_flag "$flags" ignored-only && libtest+=(--ignored)
    suite_has_flag "$flags" single-threaded && libtest+=(--test-threads=1)
    local flag
    local -a selections
    IFS=, read -r -a selections <<< "$flags"
    for flag in "${selections[@]}"; do
      if [[ "$flag" == skip=* ]]; then
        libtest+=(--skip "${flag#skip=}")
      fi
    done
    [ "${#libtest[@]}" -gt 0 ] && cmd+=(-- "${libtest[@]}")
  else
    suite_has_flag "$flags" single-threaded && cmd+=(-j1)
    suite_has_flag "$flags" nocapture && cmd+=(--no-capture)
    suite_has_flag "$flags" include-ignored && cmd+=(--run-ignored all)
    suite_has_flag "$flags" ignored-only && cmd+=(--run-ignored ignored-only)
    [ -n "$filter" ] && cmd+=(-E "test(${filter})")
  fi
  if [ "$runner" = cargo-test ]; then
    cargo_test "${cmd[@]}"
  else
    "${cmd[@]}"
  fi
}

run_uniform_store_suite() {
  local label_list filter_list filter package target runner flags
  local labels=() filters=()
  IFS='|' read -r label_list filter_list package target runner flags \
    <<<"${uniform_store_suites[$1]}"
  IFS=',' read -r -a labels <<<"$label_list"
  if [ -n "$filter_list" ]; then
    IFS=',' read -r -a filters <<<"$filter_list"
  else
    filters=("")
  fi
  if [ "${trusted}" = true ]; then
    local label
    for label in "${labels[@]}"; do
      for filter in "${filters[@]}"; do
        render_buck2_suite "$label" "$filter" "$flags"
      done
    done
  else
    for filter in "${filters[@]}"; do
      render_cargo_suite "$filter" "$package" "$target" "$runner" "$flags"
    done
  fi
}

# Every label a suite executes, for the build that precedes the service.
suite_labels() {
  if [ -n "${uniform_store_suites[$1]+set}" ]; then
    tr ',' ' ' <<<"${uniform_store_suites[$1]%%|*}"
    return
  fi
  local listed
  case "$1" in
    pg-catalog-compatibility) echo "$catalog_shape_label" "$catalog_drift_label" ;;
    pg-store)
      listed="$(labels postgres default)"
      echo "$listed" "$restate_ingress_label"
      ;;
    pg-store-synthetic-next) labels postgres synthetic-next ;;
    s3-store) labels s3 ;;
    *)
      echo "unknown store suite: $1" >&2
      return 1
      ;;
  esac
}

if [ "${labels_only}" = true ]; then
  listed="$(suite_labels "$suite")"
  tr ' ' '\n' <<<"$listed"
  exit 0
fi

if [ -n "${uniform_store_suites[$suite]+set}" ]; then
  run_uniform_store_suite "$suite"
  exit 0
fi

case "${suite}" in
  # The compatibility lanes provision the published DDL and compare its live
  # catalog rendering byte-for-byte with schema-shape.txt. The second test is a
  # distinct version-stamp gate.
  pg-catalog-compatibility)
    if [ "${trusted}" = true ]; then
      buck2_test --test_arg=committed_shape_artifact_matches_the_ddl_artifact \
        "$catalog_shape_label"
      buck2_test \
        --test_arg=a_compatible_expansion_still_reports_column_drift \
        "$catalog_drift_label"
    else
      cargo_test cargo test -p lash-internal-postgres-store --locked --lib \
        committed_shape_artifact_matches_the_ddl_artifact
      cargo_test cargo test -p lash-internal-postgres-store --locked --test schema_drift \
        a_compatible_expansion_still_reports_column_drift
    fi
    ;;

  # Package-wide by design: the integration and schema binaries are part of
  # this gate, so narrowing to the conformance binary would silently drop them.
  # The suites self-serialize on a per-process guard, and two processes on one
  # database would truncate each other's tables. Cargo runs the binaries one at
  # a time against the one database; Buck2 gives each test a slot.
  pg-store)
    if [ "${trusted}" = true ]; then
      # shellcheck disable=SC2046
      postgres_slot_test $(labels postgres default)
      postgres_slot_test \
        --test_arg=postgres_ingress \
        --test_arg=--ignored \
        "$restate_ingress_label"
    else
      cargo_test cargo test -p lash-internal-postgres-store --locked
      cargo_test cargo test -p lash-internal-restate --locked --lib postgres_ingress -- --ignored
    fi
    ;;

  # The synthetic successor's build of the same package (FIG-4262): the
  # feature-lane variants of every binary `pg-store` runs. It has a suite, a
  # service and a CI job of its own, so its shards never wait for a slot behind
  # the default build's.
  pg-store-synthetic-next)
    if [ "${trusted}" = true ]; then
      # shellcheck disable=SC2046
      postgres_slot_test $(labels postgres synthetic-next)
    else
      cargo_test cargo test -p lash-internal-postgres-store --locked --no-default-features \
        --features synthetic-next
    fi
    ;;

  s3-store)
    if [ "${trusted}" = true ]; then
      # shellcheck disable=SC2046
      buck2_test $(labels s3)
    else
      cargo_test cargo test -p lash-internal-s3-store --locked
    fi
    ;;
  *)
    echo "unknown store suite: ${suite}" >&2
    exit 1
    ;;
esac
