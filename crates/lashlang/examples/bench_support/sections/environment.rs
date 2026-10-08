pub fn benchmark_host_environment() -> &'static LashlangHostEnvironment {
    static SURFACE: OnceLock<LashlangHostEnvironment> = OnceLock::new();
    SURFACE.get_or_init(build_benchmark_host_environment)
}

#[expect(
    clippy::expect_used,
    reason = "fixture data types and operations are registered once each into a fresh catalog, so registration cannot fail, per each message"
)]
fn build_benchmark_host_environment() -> LashlangHostEnvironment {
    let mut resources = LashlangHostCatalog::tool_default(["echo", "boom", "missing_tool"]);
    resources
        .add_module_operation(
            ["jobs"],
            "Jobs",
            "run",
            "run_job",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["llm"],
            "Llm",
            "query",
            "llm_query",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["agents"],
            "Agents",
            "spawn",
            "spawn_agent",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["processes"],
            "Processes",
            "list",
            "list_process_handles",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["processes"],
            "Processes",
            "start",
            "start_process_handle",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["processes"],
            "Processes",
            "cancel",
            "cancel_process_handle",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["control"],
            "Control",
            "continue_as",
            "continue_as",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    LashlangHostEnvironment::new(resources, LashlangAbilities::all())
        .with_globals(["history", "ctx", "snap", "img", "docs", "proj"])
}

/// Lowers a benchmark program through the TypeScript front-end and links it.
///
/// ADR 0096 makes TypeScript the sole authored RLM dialect; lashlang names the
/// IR and the VM the benchmarks measure.
/// Links a benchmark scenario's program against the benchmark host surface.
#[expect(
    clippy::expect_used,
    reason = "the benchmark program links against the fixture host environment; a failure would break the fixture author's assumption"
)]
pub fn linked_benchmark_program(scenario: Scenario) -> LinkedModule {
    LinkedModule::link(benchmark_program(scenario), benchmark_host_environment())
        .expect("benchmark program should link")
}

pub fn projected_bindings(scenario: Scenario) -> ProjectedBindings {
    let mut bindings = ProjectedBindings::new();
    if !matches!(
        scenario,
        Scenario::ProjectedValues
            | Scenario::ProjectedOperations
            | Scenario::ContinueAsSeedHostEnvironment
            | Scenario::SnapshotProjectedState
    ) {
        return lashlang::testing::projection::reading_test_views(bindings);
    }
    match scenario {
        Scenario::ProjectedValues => {
            bindings.insert(
                "history",
                test_view("history", Arc::new(ProjectedList::history())),
            );
            bindings.insert(
                "docs",
                ProjectedValue::scalar("docs", projected_docs_record()),
            );
        }
        Scenario::ProjectedOperations | Scenario::ContinueAsSeedHostEnvironment => {
            bindings.insert(
                "proj",
                ProjectedValue::scalar("proj", projected_operations_record()),
            );
        }
        // The snapshot scenario seeds its projections inside a plain global
        // record: plain data that reads through the test-view provider after
        // a snapshot round-trip as before it (ADR 0132 §9), so it binds none.
        _ => {}
    }
    lashlang::testing::projection::reading_test_views(bindings)
}
