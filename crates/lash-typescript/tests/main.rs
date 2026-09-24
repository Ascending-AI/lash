#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

#[path = "advertised_tool_surface.rs"]
mod advertised_tool_surface;
#[path = "agent_surface.rs"]
mod agent_surface;
#[path = "aggregate_witnesses.rs"]
mod aggregate_witnesses;
#[path = "array_append.rs"]
mod array_append;
#[path = "assignment_in_operand.rs"]
mod assignment_in_operand;
#[path = "cell_boundary_closures.rs"]
mod cell_boundary_closures;
#[path = "closed_shape_guard.rs"]
mod closed_shape_guard;
#[path = "codemode_parity_examples.rs"]
mod codemode_parity_examples;
#[path = "console_observation.rs"]
mod console_observation;
#[path = "constructs.rs"]
mod constructs;
#[path = "depth_guard.rs"]
mod depth_guard;
#[path = "deviation_register.rs"]
mod deviation_register;
#[path = "dialect.rs"]
mod dialect;
#[path = "differential_oracle.rs"]
mod differential_oracle;
#[path = "early_errors.rs"]
mod early_errors;
#[path = "ecma_regressions.rs"]
mod ecma_regressions;
#[path = "fluency_smoke.rs"]
mod fluency_smoke;
#[path = "for_of_facets.rs"]
mod for_of_facets;
#[path = "global_this_reads.rs"]
mod global_this_reads;
#[path = "grammar_coverage.rs"]
mod grammar_coverage;
#[path = "mutable_captures.rs"]
mod mutable_captures;
#[path = "no_abort_guarantee.rs"]
mod no_abort_guarantee;
#[path = "object_string_coercion.rs"]
mod object_string_coercion;
#[path = "process_literal_captures.rs"]
mod process_literal_captures;
#[path = "projected_coercion.rs"]
mod projected_coercion;
#[path = "projected_durable_restore.rs"]
mod projected_durable_restore;
mod projected_paths;
#[path = "regex_runtime.rs"]
mod regex_runtime;
#[path = "rejections.rs"]
mod rejections;
#[path = "scoping_regressions.rs"]
mod scoping_regressions;
#[path = "session_globals.rs"]
mod session_globals;
#[path = "spread_calls.rs"]
mod spread_calls;
#[path = "structural_contract.rs"]
mod structural_contract;
#[path = "trigger_inputs.rs"]
mod trigger_inputs;
#[path = "url_runtime.rs"]
mod url_runtime;
#[path = "url_wpt.rs"]
mod url_wpt;
#[path = "value_depth_guard.rs"]
mod value_depth_guard;

#[path = "runtime_promises.rs"]
mod runtime_promises;

#[path = "console_output.rs"]
mod console_output;

#[path = "empty_list_arguments.rs"]
mod empty_list_arguments;
#[path = "execution_site_correlation.rs"]
mod execution_site_correlation;

#[path = "workflow_graph.rs"]
mod workflow_graph;
#[path = "workflow_graph_round_trip.rs"]
mod workflow_graph_round_trip;
#[path = "workflow_graph_schema.rs"]
mod workflow_graph_schema;
