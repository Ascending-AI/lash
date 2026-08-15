use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    ResourceOperationBatchResult, ResourceOperationResult, State, Value,
};

struct FluencyHost;

impl ExecutionHost for FluencyHost {
    async fn perform(&self, operation: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match operation {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult::settled_in_input_order(
                    batch
                        .operations
                        .iter()
                        .enumerate()
                        .map(|(index, _)| {
                            ResourceOperationResult::Value(Value::String(
                                format!("page-{}", index + 1).into(),
                            ))
                        })
                        .collect(),
                ),
            )),
            AbilityOp::StartProcess(_) => {
                Ok(AbilityResult::Value(Value::String("fluency-run".into())))
            }
            AbilityOp::Await(Value::String(handle)) if handle.as_str() == "fluency-run" => {
                Ok(AbilityResult::Value(Value::Number(2.0)))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected fluency operation: {other:?}"
            ))),
        }
    }
}

fn fluency_environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_binding(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            lashlang::ResourceOperationBinding {
                input_ty: lashlang::TypeExpr::Any,
                output_ty: lashlang::TypeExpr::Any,
                output_from_input: None,
            },
        )
        .expect("fluency web binding");
    lashlang::LashlangHostEnvironment::new(
        catalog,
        lashlang::LashlangAbilities::default()
            .with_sleep()
            .with_processes()
            .with_process_signals(),
    )
}

/// Executed form of the first-shot fluency corpus. These are the recurring
/// shapes the original calculator-only dialect rejected: awaited tools,
/// aggregate promises, `for...of` over tool-returned data, `map` with a
/// callback, ordinary iteration, and common data shaping. Every row links and
/// runs on the shared VM; a lowering-only success is not acceptance evidence.
///
/// The process row is narrower than the others by construction, and the doc
/// comment used to overstate it. `start` and `await` cross the effect boundary,
/// so the host answers them and the process *body* runs in a separate durable
/// execution that a cell-level host cannot drive. This row is therefore
/// evidence that the primitives lower, link and round-trip through the
/// boundary — not that `waitSignal`/`sleep`/`wake` execute. Those are executed
/// under suspension in `dialect.rs::a_process_suspended_inside_for_of_resumes`
/// and `agent_surface.rs`.
///
/// The corpus also carries one row that must be rejected. An empty hit list is
/// only evidence if the list can fill, and every earlier version of this file
/// reported an empty list without anything proving the mechanism worked.
#[test]
fn first_shot_agent_programs_execute_without_missing_methods_or_rejections() {
    let programs = [
        r#"
        const pages = await Promise.all([
          web.fetch({ url: "https://example.test/a" }),
          web.fetch({ url: "https://example.test/b" })
        ]);
        const rendered = [];
        for (let i = 0; i < pages.length; i++) { rendered[i] = JSON.stringify(pages[i]); }
        finish(rendered.join("\n"));
        "#,
        r#"
        const rows = Object.entries({ beta: 2, alpha: 1 });
        const labels = [];
        for (let i = 0; i < rows.length; i++) { labels[i] = rows[i].join(":"); }
        finish(labels.join(","));
        "#,
        r#"
        const input = ["one", "two", "three"];
        let total = 0;
        for (let i = 0; i < input.length; i++) { total = total + input[i].length; }
        finish({ total, last: input[input.length - 1].toUpperCase() });
        "#,
        r#"
        const worker = defineProcess({
          name: "worker", signals: { ready: null },
          run: async (request: unknown) => {
            const signal = await waitSignal("ready");
            await sleep(10);
            wake(signal);
            return request;
          }
        });
        finish(await start(worker, { request: Math.max(1, 2) }));
        "#,
        // `for...of` over data a tool returned: the dialect's flagship v1 guard,
        // and the shape Phase 2 of the parity runbook asks a model to write.
        // The body calls a helper, which the guard permits — only mutating,
        // aliasing or passing the iterable is refused.
        r#"
        function label(value: unknown): string { return "item:" + value; }
        const pages = await Promise.all([
          web.fetch({ url: "https://example.test/a" }),
          web.fetch({ url: "https://example.test/b" })
        ]);
        let summary = "";
        for (const page of pages) { summary = summary + label(page) + ";"; }
        finish(summary);
        "#,
        // `map` with a callback, which FIG-1305 found advertised but unusable.
        r#"
        const rows = await Promise.all([
          web.fetch({ url: "https://example.test/a" }),
          web.fetch({ url: "https://example.test/b" })
        ]);
        const shouted = rows.map((row) => row.toUpperCase());
        const indexed = rows.map((row, index) => index + ":" + row);
        finish(shouted.join(",") + "|" + indexed.join(","));
        "#,
    ];

    // The negative control. Destructuring is one of the shapes the prompt now
    // names as rejected; if this ever links and runs, the hit list has stopped
    // measuring anything.
    let rejected_control = r#"
        const source = { alpha: 1, beta: 2 };
        const { alpha, beta } = source;
        finish(alpha + beta);
    "#;

    let environment = fluency_environment();
    let mut hits = Vec::new();
    for (index, source) in programs.into_iter().enumerate() {
        let linked = match lash_typescript::link(source, &environment) {
            Ok(linked) => linked,
            Err(error) => {
                hits.push(format!("row {}: {error}", index + 1));
                continue;
            }
        };
        let outcome = futures::executor::block_on(lashlang::execute(
            &lash_typescript::compile_linked(&linked),
            &mut State::new(),
            &FluencyHost,
        ));
        match outcome {
            Ok(ExecutionOutcome::Finished(_)) => {}
            Ok(other) => hits.push(format!("row {}: unexpected {other:?}", index + 1)),
            Err(error) => hits.push(format!("row {}: {error}", index + 1)),
        }
    }
    assert!(
        hits.is_empty(),
        "first-shot missing-method/rejection hit list: {hits:#?}"
    );

    let mut control_hits = Vec::new();
    match lash_typescript::link(rejected_control, &environment) {
        Ok(linked) => {
            match futures::executor::block_on(lashlang::execute(
                &lash_typescript::compile_linked(&linked),
                &mut State::new(),
                &FluencyHost,
            )) {
                Ok(ExecutionOutcome::Finished(_)) => {}
                Ok(other) => control_hits.push(format!("control: unexpected {other:?}")),
                Err(error) => control_hits.push(format!("control: {error}")),
            }
        }
        Err(error) => control_hits.push(format!("control: {error}")),
    }
    assert_eq!(
        control_hits.len(),
        1,
        "the control must produce exactly one hit, or the hit list is not measuring"
    );
    assert!(
        control_hits[0].contains("TS_DESTRUCTURING_UNSUPPORTED"),
        "the control must be rejected for the documented reason: {control_hits:?}"
    );
}
