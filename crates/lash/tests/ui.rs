#[test]
fn model_selection_requires_model_and_variant_together() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/model_selection_requires_variant.rs");
    t.compile_fail("tests/ui/root_tool_provider_is_not_public.rs");
    t.compile_fail("tests/ui/session_admin_flat_methods_are_not_public.rs");
    t.compile_fail("tests/ui/effect_host_activation_methods_are_removed.rs");
    t.compile_fail("tests/ui/trigger_emit_requires_execution_scope.rs");
    t.compile_fail("tests/ui/core_delete_session_requires_scope.rs");
    t.compile_fail("tests/ui/session_turn_run_requires_scope.rs");
    t.compile_fail("tests/ui/session_turn_stream_requires_scope.rs");
    t.compile_fail("tests/ui/queued_turn_run_requires_scope.rs");
    t.compile_fail("tests/ui/scoped_turn_builders_are_not_prelude.rs");
    t.compile_fail("tests/ui/taxonomy_types_are_not_root.rs");
    t.compile_fail("tests/ui/process_start_requires_scope.rs");
    t.compile_fail("tests/ui/children_start_turn_is_not_public.rs");
    t.compile_fail("tests/ui/tool_state_generation_is_sealed.rs");
    t.compile_fail("tests/ui/orchestrating_tool_def_requires_unsafe.rs");
    t.compile_fail("tests/ui/orchestrating_tool_def_unsafe_is_auditable.rs");
    t.compile_fail("tests/ui/session_head_meta_is_not_serializable.rs");
    t.compile_fail("tests/ui/session_head_meta_requires_assemble.rs");
    t.compile_fail("tests/ui/commit_budget_has_no_default.rs");
    t.compile_fail("tests/ui/runtime_host_config_requires_commit_budget.rs");
    t.compile_fail("tests/ui/attempt_context_has_no_journal_capability.rs");
    t.compile_fail("tests/ui/tool_call_context_is_journal_incapable.rs");
    t.compile_fail("tests/ui/attempt_context_has_no_recursive_dispatch.rs");
    t.compile_fail("tests/ui/attempt_context_has_no_session_mutations.rs");
    t.compile_fail("tests/ui/attempt_context_has_no_trigger_commands.rs");
    t.compile_fail("tests/ui/attempt_context_has_no_process_event_commands.rs");
    t.compile_fail("tests/ui/attempt_context_has_no_child_trace_emission.rs");
    t.compile_fail("tests/ui/session_read_view_has_no_mutations.rs");
    t.compile_fail("tests/ui/pending_attempt_cannot_carry_intents.rs");
    t.compile_fail("tests/ui/pending_announcement_requires_a_replay_key.rs");
    t.compile_fail("tests/ui/after_turn_cannot_abort.rs");
    t.pass("tests/ui/facade_boundary_types_are_public.rs");
    t.pass("tests/ui/prompt_types_are_public.rs");
    t.pass("tests/ui/remote_protocol_types_are_public.rs");
    if cfg!(feature = "rlm") {
        t.pass("tests/ui/durable_builder_without_advanced.rs");
        t.pass("tests/ui/rlm_facade_boundary_types_are_public.rs");
    }
}

#[test]
fn attachment_gc_requires_explicit_root_set() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/attachment_gc_requires_explicit_root_set.rs");
}

/// The class-1 store seams carry no silent defaults: an implementor that never
/// states its delete-time freshness answer or its tombstone answer does not
/// compile.
///
/// Both fixtures assert an `E0046` whose `help:` line renders the async-trait
/// desugared signature, and rustc abbreviates `std::pin::Pin` to `Pin` in the
/// dependency-poorer no-default-features graph that the feature-boundary job
/// compiles. The expectation is therefore pinned to the workspace graph, the
/// same way the `rlm` fixtures above are.
#[test]
fn store_seam_answers_have_no_defaults() {
    if !cfg!(feature = "rlm") {
        return;
    }
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/attachment_store_head_has_no_default.rs");
    t.compile_fail("tests/ui/session_store_factory_requires_deletion_answer.rs");
}

/// The three RLM execution bounds are distinct types: a host that hands the
/// memory limit to `.instruction_limit(..)` does not compile, so the swap that
/// made `ExecutionBound::instructions(64 * 1024 * 1024)` mean "64 MiB of heap"
/// is unrepresentable.
#[test]
fn rlm_execution_bounds_are_not_swappable() {
    if !cfg!(feature = "rlm") {
        return;
    }
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/rlm_execution_bounds_are_not_swappable.rs");
}

/// The RLM config builder has no silent defaults: `build()` is absent until
/// every execution bound has been named, so a host that forgets one does not
/// compile.
#[test]
fn rlm_config_builder_requires_every_bound() {
    if !cfg!(feature = "rlm") {
        return;
    }
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/rlm_config_builder_requires_every_bound.rs");
}
