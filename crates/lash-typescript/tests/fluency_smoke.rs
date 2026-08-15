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
/// aggregate promises, ordinary iteration, common data shaping, and durable
/// process primitives. Every row now links and runs on the shared VM; a
/// lowering-only success is not acceptance evidence.
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
          run: async (input: unknown) => {
            const signal = await waitSignal("ready");
            await sleep(10);
            wake(signal);
            return input;
          }
        });
        finish(await start(worker, { input: Math.max(1, 2) }));
        "#,
    ];

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
}
