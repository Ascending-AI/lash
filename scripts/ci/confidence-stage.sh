#!/usr/bin/env bash
# Sourced by confidence-gate.sh after parsing the full lane and defining its
# functions. Each stage owns disjoint work from the full driver below it.
if [ "$requested_selector" != full ]; then
  echo 'Confidence stages require the unscoped full selector' >&2
  exit 2
fi
case "$LASH_CONFIDENCE_STAGE" in
  build)
    bootstrap_tools
    cargo test --workspace --all-targets --locked --no-run
    cargo build --workspace --bins --locked
    cargo build --workspace --examples --locked
    cargo build --locked --release -p lash-restate-postgres-workers-e2e --bins
    ;;
  harnesses)
    run_scenario_harnesses
    run_state_machine_and_fault_matrix
    run_sim_unit_suite
    write_provider_transport_exclusion_evidence
    write_sim_lane_declarations
    write_full_lane_prerequisites
    write_postgres_effect_history_status
    write_restate_postgres_workers_e2e_lane_status
    ;;
  generated) run_sim_generated_lane ;;
  minimizer)
    run_minimizer_fixture_suite
    run_focused_sqlite_seed_tail_repro
    ;;
  backends)
    run_local_backend_conformance
    run_backend_contention_evidence
    run_current_postgres_trace_replay_evidence
    run_postgres_conformance
    ;;
  workers) run_restate_postgres_workers_e2e ;;
  coverage) run_coverage_blind_spots ;;
  mutation-core)
    run_lash_core_direct_model_mutation_evidence
    test "$mutation_commands_run" -gt 0
    test "$mutation_failures" -eq 0
    ;;
  mutation-sim)
    run_lash_sim_runtime_completion_mutation_evidence
    test "$mutation_commands_run" -gt 0
    test "$mutation_failures" -eq 0
    ;;
  mutation-packages)
    case "${LASH_CONFIDENCE_PACKAGE:-}" in
      lash-internal-core|lash-internal-lashlang|lash-internal-protocol-rlm|lash-internal-protocol-standard|lash-internal-sqlite-store|lash-internal-postgres-store) ;;
      *) echo 'Unknown full mutation package' >&2; exit 2 ;;
    esac
    selected_packages=("$LASH_CONFIDENCE_PACKAGE")
    # Each leg is one job inside the 100-minute cap; the mutant space cannot
    # be swept whole, so the stage judges bounded slices (sized by
    # MUTATION_PACKAGES_* in scripts/confidence-gate.sh). The workflow's
    # matrix legs hand their coordinate down as LASH_MUTATION_PACKAGES_SHARD
    # and the slice index rotates across runs via LASH_MUTATION_RUN_INDEX.
    LASH_MUTATION_PACKAGES_BOUNDED=1
    run_mutation_smoke
    run_mutation_full
    finalize_mutation_gate
    ;;
  *) echo "Unknown confidence stage: $LASH_CONFIDENCE_STAGE" >&2; exit 2 ;;
esac
assert_no_panics_in_artifacts
printf '{"stage":"%s","status":"passed"}\n' "$LASH_CONFIDENCE_STAGE" > "${out_dir}/stage.json"
