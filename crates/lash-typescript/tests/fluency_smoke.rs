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
                        .map(|(index, call)| {
                            ResourceOperationResult::Value(resource_operation_value(call, index))
                        })
                        .collect(),
                ),
            )),
            AbilityOp::ResourceOperation(call) => {
                Ok(AbilityResult::Value(resource_operation_value(&call, 0)))
            }
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

/// Answers one host resource operation.
///
/// `Date.now()` and `Math.random()` are journaled reads on the
/// `__typescript_runtime` resource and `registerTrigger` is a call on the
/// trigger registry — host operations rather than language builtins, so the
/// host has to answer each in its own declared shape. Everything else is a
/// tool call and gets the corpus's page payload.
fn resource_operation_value(call: &lashlang::ResourceOperation, index: usize) -> Value {
    let alias = match &call.receiver {
        Value::Resource(handle) => handle.alias.as_str(),
        _ => "",
    };
    match (alias, call.operation.as_str()) {
        ("__typescript_runtime", "now") => Value::Number(1_700_000_000_000.0),
        ("__typescript_runtime", "random") => Value::Number(0.5),
        ("triggers", "register") => trigger_registration_value(),
        _ => Value::String(format!("page-{}", index + 1).into()),
    }
}

/// A registration record shaped like the one `triggers.register` returns.
fn trigger_registration_value() -> Value {
    let mut record = lashlang::Record::new();
    record.insert(
        "subscription_key".to_string(),
        Value::String("fluency-subscription".into()),
    );
    record.insert("incarnation".to_string(), Value::String("1".into()));
    record.insert("revision".to_string(), Value::Number(1.0));
    record.insert(
        "registrant".to_string(),
        Value::Record(std::sync::Arc::new(lashlang::Record::new())),
    );
    record.insert(
        "manifest_membership".to_string(),
        Value::String("present_in_current_artifact".into()),
    );
    record.insert(
        "source_key".to_string(),
        Value::String("timer.Schedule".into()),
    );
    record.insert("name".to_string(), Value::String("fluency-trigger".into()));
    record.insert(
        "source_type".to_string(),
        Value::String("timer.Schedule".into()),
    );
    record.insert(
        "source".to_string(),
        Value::Record(std::sync::Arc::new(lashlang::Record::new())),
    );
    record.insert(
        "target".to_string(),
        Value::Record(std::sync::Arc::new(lashlang::Record::new())),
    );
    record.insert("enabled".to_string(), Value::Bool(true));
    Value::Record(std::sync::Arc::new(record))
}

/// The corpus's host environment.
///
/// This mirrors what the real RLM host builds in
/// `lash_lashlang_runtime::lashlang_host_environment_from_tool_catalog`: the
/// `__typescript_runtime` `now`/`random` bindings behind `Date.now()` and
/// `Math.random()`, and — with triggers enabled — the trigger resource
/// operations behind `registerTrigger`. A bare catalog cannot reach any of the
/// three, so a corpus built on one was silently unable to exercise three
/// behaviours the production prompt advertises.
fn fluency_environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    for (operation, host_operation) in [
        ("now", "typescript.runtime.now"),
        ("random", "typescript.runtime.random"),
    ] {
        catalog
            .add_module_operation_binding(
                ["__typescript_runtime"],
                "typescript.Runtime",
                operation,
                host_operation,
                lashlang::ResourceOperationBinding {
                    input_ty: lashlang::TypeExpr::Any,
                    output_ty: lashlang::TypeExpr::Float,
                    output_from_input: None,
                },
            )
            .expect("fluency typescript runtime binding");
    }
    lashlang::add_trigger_resource_operations(&mut catalog);
    catalog
        .add_trigger_source_constructor(
            ["timer", "Schedule"],
            lashlang::TypeExpr::Object(vec![lashlang::TypeField {
                name: "expr".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            }]),
            lashlang::NamedDataType::object(
                "timer.Tick",
                vec![lashlang::TypeField {
                    name: "fired_at".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: false,
                }],
            )
            .expect("valid fluency timer tick type"),
        )
        .expect("fluency timer trigger source");
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
            .with_process_signals()
            .with_triggers(),
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
        // The journaled runtime reads. Both are named in the production prompt
        // and both are host operations, not language builtins, so a corpus
        // against a bare catalog could not reach either.
        r#"
        const startedAt = Date.now();
        const jitter = Math.random();
        finish({ startedAt, bounded: jitter >= 0 && jitter <= 1 });
        "#,
        // `registerTrigger`, the third advertised behaviour the bare catalog
        // could not reach: it needs the trigger resource operations and a
        // trigger source constructor.
        r#"
        const remember = defineProcess({
          name: "remember", signals: {},
          run: async (tick: unknown) => { return tick; }
        });
        const source = timer.Schedule({ expr: "0 8 * * *" });
        const registration = await registerTrigger({
          source,
          target: remember,
          inputs: { tick: trigger.event },
          name: "fluency-trigger"
        });
        finish(registration.enabled);
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
