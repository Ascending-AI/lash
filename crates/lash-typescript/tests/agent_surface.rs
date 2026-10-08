use lashlang::{
    AbilityOp, AbilityOutcome, Declaration, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    Expr, ResourceOperationBatchOutcome, ResourceOperationOutcome, State, Value, Vm, VmRunOutcome,
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "agent_surface/fig3463_observation.rs"]
mod fig3463_observation;
#[path = "agent_surface/journaled_randomness.rs"]
mod journaled_randomness;
#[path = "agent_surface/process_handle_containers.rs"]
mod process_handle_containers;

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityOutcome::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unexpected agent-surface ability")),
        }
    }
}

/// The host environment every process fixture links against.
///
/// FIG-2999 deleted `defineProcess`, `start` and `wake`: a process is an
/// uncalled top-level `const` async arrow, and the controls are leaf tools, so
/// a fixture starts one through `processes.*`. The `process` slot
/// is typed `Process` through `x-lash`, which is what the linker lifts the
/// literal into.
fn process_environment() -> lashlang::LashlangHostEnvironment {
    process_environment_with(lashlang::LashlangHostCatalog::new())
}

fn process_environment_with(
    mut catalog: lashlang::LashlangHostCatalog,
) -> lashlang::LashlangHostEnvironment {
    catalog
        .add_module_operation_contract(
            ["processes"],
            "Processes",
            "start",
            "tool:processes/start",
            &lashlang::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": true,
                    "properties": { "definition": { "x-lash": { "kind": "process_unknown" } } },
                    "required": ["definition"]
                }),
                serde_json::json!({ "x-lash": { "kind": "process_unknown" } }),
            ),
        )
        .expect("process start operation");

    lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::all())
}

pub(super) fn finished(source: &str) -> Value {
    let program = lash_typescript::testing::compile(source).expect("TypeScript should compile");
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
        .expect("TypeScript should execute")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

#[test]
fn process_membership_reaches_functions_nested_inside_run() {
    let error = lash_typescript::parse(
        r#"
        const worker = async () => {
          function stop() { finish(1); }
          return stop();
        };
        const handle = await processes.start({ definition: worker });
        finish(handle);
        "#,
    )
    .expect_err("finish nested inside run must remain cell-only");
    assert_eq!(
        error.code,
        lash_typescript::DiagnosticCode::UnsupportedExpression
    );
    assert!(error.message.contains("cell-only"));
}

/// The JSON handle record a real host mints for the process a fixture labels.
fn process_handle_json(label: &str) -> serde_json::Value {
    let process_id = lash_sansio::ProcessId::fixture(label);
    serde_json::json!({
        "__handle__": "lash",
        "id": lash_sansio::handle::HandleId::process(&process_id).as_str(),
        "process_id": process_id.as_str(),
    })
}

/// The handle record a real host mints for a started process; a bare string
/// is a resolved value, and awaiting one is a guest error.
fn process_handle(label: &str) -> Value {
    let mut handle = lashlang::Record::new();
    handle.insert("__handle__".to_string(), Value::String("lash".into()));
    let process_id = lash_sansio::ProcessId::fixture(label);
    handle.insert(
        "id".to_string(),
        Value::String(
            lash_sansio::handle::HandleId::process(&process_id)
                .as_str()
                .into(),
        ),
    );
    handle.insert(
        "process_id".to_string(),
        Value::String(process_id.as_str().into()),
    );
    Value::Record(std::sync::Arc::new(handle))
}

enum ProcessAwaitFailureHost {
    Typed,
    MessageOnly,
}

impl ExecutionHost for ProcessAwaitFailureHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) => {
                Ok(AbilityOutcome::Value(process_handle("rejected-run")))
            }
            AbilityOp::Await(handle) if handle == process_handle("rejected-run") => match self {
                Self::Typed => Err(ExecutionHostError::from_tool_failure(
                    &lash_sansio::ToolFailure {
                        cause: None,
                        class: lash_sansio::ToolFailureClass::PermissionDenied,
                        code: "approval_denied".to_string(),
                        message: "approval was denied".to_string(),
                        source: lash_sansio::ToolFailureSource::Policy,
                        suggested_delay_ms: None,
                        raw: None,
                    },
                    "await-effect-key",
                )),
                Self::MessageOnly => Err(ExecutionHostError::new("plain await failure")),
            },
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unexpected process-await rejection ability",
            )),
        }
    }
}

fn caught_process_await(host: &impl ExecutionHost, probe: &str) -> Value {
    let source = format!(
        r#"
        const worker = async () => {{ return null; }};
        const handle = await processes.start({{ definition: worker }});
        try {{
          await handle;
          finish("the process await did not fail");
        }} catch (error) {{
          finish({probe});
        }}
        "#
    );
    let linked =
        lash_typescript::link(&source, &process_environment()).expect("process await should link");
    match futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        host,
    ))
    .expect("the process-await failure is catchable")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected caught process await to finish, got {other:?}"),
    }
}

#[test]
fn direct_process_handle_await_preserves_typed_tool_failure_fields() {
    assert_eq!(
        caught_process_await(
            &ProcessAwaitFailureHost::Typed,
            r#"{
              caught: error instanceof Error,
              name: error.name,
              code: error.cause.code,
              message: error.message,
              class: error.cause.class,
              source: error.cause.source
            }"#,
        ),
        lashlang::from_json(serde_json::json!({
            "caught": true,
            "name": "EffectError",
            "code": "approval_denied",
            "message": "approval was denied",
            "class": "permission_denied",
            "source": "policy"
        }))
    );
}

#[test]
fn direct_process_handle_await_keeps_message_only_error_shape() {
    assert_eq!(
        caught_process_await(
            &ProcessAwaitFailureHost::MessageOnly,
            "[error.message, error.cause.code, error.cause.details.kind, error.cause.details.operation]",
        ),
        lashlang::from_json(serde_json::json!([
            "`?` unwrapped failed tool result: plain await failure",
            "UnwrappedToolResultFailed",
            "effect",
            "await"
        ]))
    );
}

fn find_receiver_call(expr: &Expr) -> Option<(Vec<&str>, &str)> {
    match expr {
        Expr::ReceiverCall {
            receiver,
            operation,
            ..
        } => {
            if let Expr::ResourceRef(resource_ref) = receiver.as_ref() {
                Some((
                    resource_ref.path.iter().map(|s| s.as_str()).collect(),
                    operation.as_str(),
                ))
            } else {
                None
            }
        }
        _ => expr.children().find_map(find_receiver_call),
    }
}

/// Which methods a literal receiver carries used to be a hand-written table
/// beside the signature table, and the two disagreed: `valueOf` was listed for
/// string, number and the remaining literals but not for arrays, so
/// `[1].valueOf()` was refused as unavailable on this literal receiver while
/// the same call on a bound array lowered and ran (FIG-1718). Both spellings
/// are the same call and must answer the same.
#[test]
fn array_literal_value_of_matches_the_bound_array_path() {
    let expected = Value::List(vec![Value::Number(1.0), Value::Number(2.0)].into());
    assert_eq!(finished("finish([1, 2].valueOf());"), expected);
    assert_eq!(
        finished("const items = [1, 2]; finish(items.valueOf());"),
        expected
    );
}

/// `Array.prototype.valueOf` hands back the receiver, not a copy of it. The
/// value is the weaker half of the claim — a detached copy satisfies the
/// assertion above — so pin the two properties only identity has: a write
/// through the result reaches the original, and `===` holds. `slice()` is the
/// control, a genuine copy that must fail both.
#[test]
fn array_value_of_returns_the_receiver_rather_than_a_copy() {
    assert_eq!(
        finished("const a = [1, 2]; const b = a.valueOf(); b.push(3); finish(a.length);"),
        Value::Number(3.0),
        "a write through valueOf() must reach the receiver"
    );
    assert_eq!(
        finished("const a = [1, 2]; finish(a.valueOf() === a);"),
        Value::Bool(true)
    );
    assert_eq!(
        finished("const a = [1, 2]; const b = a.slice(); b.push(3); finish(a.length);"),
        Value::Number(2.0),
        "slice() is a copy, so the control must not alias"
    );
    assert_eq!(
        finished("const a = [1, 2]; finish(a.slice() === a);"),
        Value::Bool(false)
    );
}

/// The ticket repro is a guest-level RegExp match: preserve its array-shaped
/// result fields while `valueOf()` returns the same match object.
#[test]
fn regexp_match_value_of_preserves_guest_shape_and_identity() {
    assert_eq!(
        finished(
            r#"
            const matched = "abc".match(/b/).valueOf();
            finish({ index: matched.index, first: matched[0], same: matched.valueOf() === matched });
            "#,
        ),
        lashlang::from_json(serde_json::json!({
            "index": 1,
            "first": "b",
            "same": true
        }))
    );
}

#[test]
fn instance_stdlib_collision_matrix_guard_sweeps_all_stdlib_methods() {
    let methods = lash_typescript::accepted_instance_methods();
    assert_eq!(
        methods.len(),
        94,
        "the collision matrix must sweep every accepted instance method"
    );

    for method in methods {
        // 1. Single-segment module authority: `await tools.<method>({})`
        let source_single = format!(r#"finish(await tools.{method}({{ payload: 1 }}));"#);
        let program_single = lash_typescript::parse(&source_single)
            .unwrap_or_else(|error| panic!("tools.{method} should parse: {error}"));
        let (path_single, op_single) = find_receiver_call(&program_single.main)
            .unwrap_or_else(|| panic!("expected ReceiverCall for tools.{method}"));
        assert_eq!(path_single, &["tools"], "tools.{method} path");
        assert_eq!(op_single, *method, "tools.{method} op");

        // 2. Dotted module authority: `await inbox.alpha.<method>({})`
        let source_dotted = format!(r#"finish(await inbox.alpha.{method}({{ payload: 1 }}));"#);
        let program_dotted = lash_typescript::parse(&source_dotted)
            .unwrap_or_else(|error| panic!("inbox.alpha.{method} should parse: {error}"));
        let (path_dotted, op_dotted) = find_receiver_call(&program_dotted.main)
            .unwrap_or_else(|| panic!("expected ReceiverCall for inbox.alpha.{method}"));
        assert_eq!(
            path_dotted,
            &["inbox", "alpha"],
            "inbox.alpha.{method} path"
        );
        assert_eq!(op_dotted, *method, "inbox.alpha.{method} op");
    }

    // Counter-case: an ECMA global namespace root with an instance-stdlib method
    // (globalThis.missing.get) must NOT lower as a tool call.
    let counter_source = "finish(globalThis.missing.get('k'));";
    let counter_program = lash_typescript::parse(counter_source)
        .expect("globalThis.missing.get should parse without requiring await");
    assert!(
        find_receiver_call(&counter_program.main).is_none(),
        "globalThis.missing.get must not lower as a tool call"
    );
}

struct AggregateHost;

impl ExecutionHost for AggregateHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityOutcome::ResourceOperationBatch(
                batch.answer_in_leaf_order(
                    batch
                        .leaves
                        .iter()
                        .filter_map(lashlang::ResourceOperationBatchLeaf::operation)
                        .enumerate()
                        .map(|(index, _)| {
                            ResourceOperationOutcome::Value(Value::Number(index as f64 + 1.0))
                        })
                        .collect(),
                ),
            )),
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected aggregate ability")),
        }
    }
}

struct SettledHost;

impl ExecutionHost for SettledHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityOutcome::ResourceOperationBatch(
                batch.answer_in_leaf_order(
                    batch
                        .leaves
                        .iter()
                        .filter_map(lashlang::ResourceOperationBatchLeaf::operation)
                        .enumerate()
                        .map(|(index, _)| {
                            if index == 0 {
                                ResourceOperationOutcome::Value(Value::String("ok".into()))
                            } else {
                                ResourceOperationOutcome::Error(ExecutionHostError::new("boom"))
                            }
                        })
                        .collect(),
                ),
            )),
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected settled ability")),
        }
    }
}

struct SequentialAsyncMapHost {
    calls: AtomicUsize,
}

impl ExecutionHost for SequentialAsyncMapHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) => {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(AbilityOutcome::Value(Value::String("ok".into())))
                } else {
                    Err(ExecutionHostError::new("boom"))
                }
            }
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unexpected sequential async-map ability",
            )),
        }
    }
}

#[test]
fn promise_all_settled_async_map_catches_each_effect_failure_and_continues() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "finish(await Promise.allSettled(['a','b'].map(async url => await web.fetch({url}))));",
        &environment,
    )
    .expect("allSettled async map should link");
    let host = SequentialAsyncMapHost {
        calls: AtomicUsize::new(0),
    };
    let outcome = futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &host,
    ))
    .expect("an individual callback rejection must not abort the async map");
    assert_eq!(host.calls.load(Ordering::SeqCst), 2);
    let ExecutionOutcome::Finished(Value::List(items)) = outcome else {
        panic!("allSettled async map returns a list, got {outcome:?}");
    };
    assert_eq!(items.len(), 2);
    let rendered = format!("{items:?}");
    assert!(rendered.contains("fulfilled"), "{rendered}");
    assert!(rendered.contains("rejected"), "{rendered}");
    assert!(rendered.contains("boom"), "{rendered}");
}

#[test]
fn promise_all_executes_on_the_shared_aggregate_batch_machine() {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            &lashlang::OperationContract::new(serde_json::json!({}), serde_json::json!({})),
        )
        .expect("test host binding");
    let environment =
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default());
    let linked = lash_typescript::link(
        "const results = await Promise.all([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]); finish(results);",
        &environment,
    )
    .expect("Promise.all tool calls should link");
    let compiled = lashlang::testing::harness::compile_linked_main(&linked);
    let outcome = futures::executor::block_on(lashlang::execute(
        &compiled,
        &mut State::new(),
        &AggregateHost,
    ))
    .expect("aggregate should execute");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(1.0), Value::Number(2.0)].into()
        ))
    );
}

#[test]
fn promise_all_settled_preserves_javascript_result_shape() {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            &lashlang::OperationContract::new(serde_json::json!({}), serde_json::json!({})),
        )
        .expect("test host binding");
    let environment =
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default());
    let linked = lash_typescript::link(
        "finish(await Promise.allSettled([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]));",
        &environment,
    )
    .expect("Promise.allSettled tool calls should link");
    let outcome = futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &SettledHost,
    ))
    .expect("settled aggregate should execute");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!([
            { "status": "fulfilled", "value": "ok" },
            { "status": "rejected", "reason": {
                "name": "EffectError",
                "message": "boom",
                "cause": {
                    "code": "ResourceOperationFailed",
                    "details": { "kind": "effect", "operation": "resource_batch" }
                }
            } }
        ])))
    );
}

/// A rejected `allSettled` leaf's reason is the same idiomatic error the awaited
/// form throws, so the discrimination a model writes against it works there too.
#[test]
fn promise_all_settled_rejection_reason_is_an_idiomatic_error() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "const results = await Promise.allSettled([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]);
         const reason = results[1].reason;
         finish([reason instanceof Error, String(reason), reason.name, reason.cause.code]);",
        &environment,
    )
    .expect("Promise.allSettled tool calls should link");
    let outcome = futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &SettledHost,
    ))
    .expect("settled aggregate should execute");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!([
            true,
            "EffectError: boom",
            "EffectError",
            "ResourceOperationFailed"
        ])))
    );
}

/// A host whose only tool operation fails, so a cell can catch the rejection the
/// substrate delivers for an ordinary awaited tool call.
struct RejectingToolHost;

impl ExecutionHost for RejectingToolHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) => Err(ExecutionHostError::new("boom")),
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityOutcome::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unexpected rejection ability")),
        }
    }
}

fn caught_rejection(probe: &str) -> Value {
    let environment = two_leaf_web_environment();
    let source = format!(
        "try {{ await web.fetch({{ url: 'a' }}); finish('the tool call did not fail'); }}
         catch (error) {{ finish({probe}); }}"
    );
    let linked = lash_typescript::link(&source, &environment).expect("probe should link");
    match futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &RejectingToolHost,
    ))
    .expect("a failing tool call is catchable")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

/// FIG-1477's first observed failure: the delivered rejection was a plain
/// record, so the `instanceof Error` guard every model writes took the wrong
/// branch.
#[test]
fn a_tool_rejection_is_an_instance_of_error() {
    assert_eq!(
        caught_rejection("error instanceof Error"),
        Value::Bool(true)
    );
    assert_eq!(
        caught_rejection("[error instanceof TypeError, error instanceof RangeError]"),
        lashlang::from_json(serde_json::json!([false, false])),
        "the brand is an Error and nothing narrower"
    );
}

/// FIG-1477's second observed failure: `String(error)` rendered
/// `[object Object]`, so a model logging or reporting the rejection lost the
/// host's own text.
#[test]
fn a_tool_rejection_stringifies_informatively() {
    let Value::String(rendered) = caught_rejection("String(error)") else {
        panic!("String(error) is a string");
    };
    assert!(
        rendered.starts_with("EffectError: "),
        "String(error) names the brand: {rendered}"
    );
    assert!(
        rendered.contains("boom"),
        "String(error) carries the host's own text: {rendered}"
    );
}

/// FIG-1477's third observed failure: the standard try/catch discrimination —
/// read `message` off an `Error`, fall back to `String` otherwise — produced the
/// fallback branch and a wrong judged answer.
#[test]
fn a_tool_rejection_answers_the_standard_discrimination_pattern() {
    let Value::String(rendered) = caught_rejection(
        "error instanceof Error ? error.message : `not an error: ${String(error)}`",
    ) else {
        panic!("the discrimination probe finishes a string");
    };
    assert!(
        rendered.contains("boom"),
        "the Error branch reports the message: {rendered}"
    );
    assert_eq!(
        caught_rejection(
            "[error.name, typeof error.message, error.cause.code, error.cause.details.kind]"
        ),
        lashlang::from_json(serde_json::json!([
            "EffectError",
            "string",
            "UnwrappedModuleOperationFailed",
            "effect"
        ])),
        "the typed payload stays reachable on the documented `cause` property"
    );
}

#[test]
fn promise_aggregates_apply_promise_resolve_to_plain_values() {
    assert_eq!(
        finished("finish(await Promise.all([1, 2]));"),
        lashlang::from_json(serde_json::json!([1, 2]))
    );
    assert_eq!(
        finished("finish(await Promise.allSettled([1, 2]));"),
        lashlang::from_json(serde_json::json!([
            { "status": "fulfilled", "value": 1 },
            { "status": "fulfilled", "value": 2 }
        ]))
    );
}

struct RuntimeValueHost;

impl ExecutionHost for RuntimeValueHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                let Value::Resource(receiver) = operation.receiver else {
                    return Err(ExecutionHostError::new(
                        "runtime receiver is not a resource",
                    ));
                };
                assert_eq!(
                    receiver.resource_type.as_str(),
                    lashlang::LANGUAGE_RUNTIME_RESOURCE_TYPE
                );
                assert_eq!(receiver.alias.as_str(), "builtin");
                assert!(operation.args.is_empty());
                match operation.operation.as_str() {
                    lashlang::LANGUAGE_RUNTIME_NOW_OPERATION => {
                        Ok(AbilityOutcome::Value(Value::Number(1_723_456.0)))
                    }
                    lashlang::LANGUAGE_RUNTIME_RANDOM_OPERATION => {
                        Ok(AbilityOutcome::Value(Value::Number(0.25)))
                    }
                    other => Err(ExecutionHostError::new(format!(
                        "unexpected runtime operation {other}"
                    ))),
                }
            }
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected runtime-value ability")),
        }
    }
}

#[test]
fn time_and_randomness_are_host_effects_instead_of_vm_nondeterminism() {
    let lowered = lash_typescript::parse("finish([Date.now(), Math.random()]);")
        .expect("runtime values should lower");
    assert!(
        lashlang::referenced_module_call_paths(&lowered).is_empty(),
        "resolved runtime intrinsics must not enter deferred tool discovery"
    );
    let program =
        lash_typescript::testing::compile("finish({ now: Date.now(), random: Math.random() });")
            .expect("runtime values should compile");
    let outcome = futures::executor::block_on(lashlang::execute(
        &program,
        &mut State::new(),
        &RuntimeValueHost,
    ))
    .expect("runtime values should execute through the host");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!({
            "now": 1_723_456,
            "random": 0.25
        })))
    );
}

#[test]
fn argless_date_uses_the_same_journaled_clock_effect_as_date_now() {
    let program = lash_typescript::testing::compile(
        "const d=new Date(); finish(`${d.getTime()}|${Date.now()}|${d.toISOString()}`);",
    )
    .expect("argless Date should compile through the runtime clock");
    let outcome = futures::executor::block_on(lashlang::execute(
        &program,
        &mut State::new(),
        &RuntimeValueHost,
    ))
    .expect("argless Date should execute through the host");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::String(
            "1723456|1723456|1970-01-01T00:28:43.456Z".into()
        ))
    );
}

struct ProcessDurabilityHost;

impl ExecutionHost for ProcessDurabilityHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityOutcome::ResourceOperationBatch(
                batch.answer_in_leaf_order(
                    batch
                        .leaves
                        .iter()
                        .filter_map(lashlang::ResourceOperationBatchLeaf::operation)
                        .map(|operation| {
                            ResourceOperationOutcome::Value(
                                operation
                                    .args
                                    .first()
                                    .and_then(Value::as_record)
                                    .and_then(|record| record.get("value"))
                                    .cloned()
                                    .unwrap_or(Value::Null),
                            )
                        })
                        .collect(),
                ),
            )),
            AbilityOp::Sleep(_) => Ok(AbilityOutcome::Value(Value::Null)),

            // A start names the process it is asked to start: the fixture's
            // process values carry a `name`, so a handle minted here can be
            // told apart from a handle minted for another process.
            AbilityOp::ResourceOperation(call) => {
                let name = call
                    .args
                    .first()
                    .and_then(Value::as_record)
                    .and_then(|record| record.get("definition"))
                    .and_then(Value::as_record)
                    .and_then(|record| record.get("name"))
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| "worker".to_string());
                Ok(AbilityOutcome::Value(lashlang::from_json(
                    process_handle_json(&name),
                )))
            }
            AbilityOp::Await(handle) => {
                let id = handle
                    .as_record()
                    .and_then(|record| record.get("process_id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(AbilityOutcome::Value(Value::String(
                    format!("{id} awaited").into(),
                )))
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unexpected durable-process ability",
            )),
        }
    }
}

/// The lifted process a fixture drives, named by its parameter count.
///
/// FIG-2999: a process literal's name is the linker's lift identity, not an
/// authored string, so a fixture that lifts more than one process picks the
/// one it means by shape instead of by name.
fn lifted_process_name(linked: &lashlang::LinkedModule, params: usize) -> String {
    linked
        .artifact
        .ir()
        .declarations
        .iter()
        .find_map(|declaration| match declaration {
            Declaration::Process(process) if process.params.len() == params => {
                Some(process.name.to_string())
            }
            _ => None,
        })
        .expect("the module lifts a process of that shape")
}

fn suspend_and_resume_process(
    source: &str,
    globals: serde_json::Value,
    params: usize,
) -> ExecutionOutcome {
    futures::executor::block_on(async {
        let mut catalog = lashlang::LashlangHostCatalog::new();
        catalog
            .add_module_operation_contract(
                ["web"],
                "Web",
                "fetch",
                "tool:web/fetch",
                &lashlang::OperationContract::new(serde_json::json!({}), serde_json::json!({})),
            )
            .expect("test host binding");
        let linked = lash_typescript::link(source, &process_environment_with(catalog))
            .expect("process should link");
        // FIG-2999: the literal's name is the linker's lift name, not an
        // authored one, so the fixture asks the artifact for the process it
        // lifted rather than spelling a name the source no longer carries.
        let process_name = lifted_process_name(&linked, params);
        let compiled =
            lashlang::testing::harness::compile_linked_process_named(&linked, &process_name)
                .expect("process should compile");
        let mut state = State::from_snapshot(lashlang::Snapshot::new(
            lashlang::from_json(globals)
                .as_record()
                .expect("process globals must be a record")
                .clone(),
        ));
        let host = ProcessDurabilityHost;
        let execution_environment = lashlang::ExecutionEnvironment::new(&host).process();
        let mut vm = Vm::from_state(&compiled, &mut state, &execution_environment)
            .expect("install process VM");
        assert_eq!(
            vm.run_process_until_effect()
                .await
                .expect("run to durable effect"),
            VmRunOutcome::EffectCompleted
        );
        let encoded = serde_json::to_vec(&vm.suspend().expect("capture continuation"))
            .expect("encode continuation");
        let continuation = lashlang::VmInstance::pristine()
            .open_continuation(&encoded)
            .expect("decode continuation");
        let mut resumed = Vm::resume_from(continuation, &compiled, &execution_environment)
            .expect("resume process VM");
        loop {
            match resumed
                .run_process_until_effect()
                .await
                .expect("complete resumed process")
            {
                VmRunOutcome::EffectCompleted => {}
                VmRunOutcome::HandedOver => panic!("this host never hands an effect over"),
                VmRunOutcome::Complete(outcome) => break outcome,
            }
        }
    })
}

#[test]
fn uncaught_throw_fails_a_durable_process() {
    futures::executor::block_on(async {
        let source = r#"
            const worker = async () => { throw "broken"; };
            finish(await processes.start({ definition: worker }));
        "#;
        let linked =
            lash_typescript::link(source, &process_environment()).expect("process should link");
        let process_name = lifted_process_name(&linked, 0);
        let compiled =
            lashlang::testing::harness::compile_linked_process_named(&linked, &process_name)
                .expect("process compiles");
        let mut state = State::new();
        let host = ProcessDurabilityHost;
        let execution_environment = lashlang::ExecutionEnvironment::new(&host).process();
        let mut vm = Vm::from_state(&compiled, &mut state, &execution_environment)
            .expect("install process VM");
        let outcome = match vm
            .run_process_until_effect()
            .await
            .expect("uncaught throw should become a process outcome")
        {
            VmRunOutcome::Complete(outcome) => outcome,
            VmRunOutcome::EffectCompleted => panic!("process failure should be terminal"),
            VmRunOutcome::HandedOver => panic!("this host never hands an effect over"),
        };
        assert_eq!(
            outcome,
            ExecutionOutcome::Failed(Value::String("broken".into()))
        );
    });
}

#[test]
fn durable_process_resumes_after_shared_promise_batch() {
    let source = r#"
        const worker = async () => await Promise.all([
          web.fetch({ value: 1 }), web.fetch({ value: 2 })
        ]);
        finish(await processes.start({ definition: worker }));
    "#;
    assert_eq!(
        suspend_and_resume_process(source, serde_json::json!({}), 0),
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(1.0), Value::Number(2.0)].into()
        ))
    );
}

/// The decisive case from the FIG-1305 report.
///
/// Leaf 0 rejects late with `late-A`; leaf 1 rejects early with `early-B`. The
/// host consumed leaf 1's rejection first. ECMA rejects `Promise.all` at the
/// first settled rejection, and under ADR 0099 §10 the host answers `all`
/// with exactly that settlement — so the surfaced reason must be `early-B`,
/// and the VM never sees `late-A` at all. `allSettled` asks the same host for
/// every result, in input order.
struct FirstSettledRejectionHost;

impl ExecutionHost for FirstSettledRejectionHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => {
                assert_eq!(batch.leaves.len(), 2, "the decisive case has two leaves");
                Ok(AbilityOutcome::ResourceOperationBatch(
                    match batch.consumer {
                        // Leaf 1 settled first.
                        lashlang::AggregateConsumer::All => {
                            ResourceOperationBatchOutcome::Selected {
                                leaf: 1,
                                result: ResourceOperationOutcome::Error(ExecutionHostError::new(
                                    "early-B",
                                )),
                            }
                        }
                        lashlang::AggregateConsumer::AllSettled => {
                            ResourceOperationBatchOutcome::AllResults(vec![
                                ResourceOperationOutcome::Error(ExecutionHostError::new("late-A")),
                                ResourceOperationOutcome::Error(ExecutionHostError::new("early-B")),
                            ])
                        }
                        other => {
                            panic!("the decisive case asks for all or allSettled, not {other:?}")
                        }
                    },
                ))
            }
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected first-settled ability")),
        }
    }
}

fn two_leaf_web_environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            &lashlang::OperationContract::new(serde_json::json!({}), serde_json::json!({})),
        )
        .expect("test host binding");
    lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default())
}

/// [`two_leaf_web_environment`] plus the process control tools.
fn mixed_aggregate_environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            &lashlang::OperationContract::new(serde_json::json!({}), serde_json::json!({})),
        )
        .expect("test host binding");
    process_environment_with(catalog)
}

#[test]
fn promise_all_rejects_with_the_rejection_its_host_consumed_first() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "const results = await Promise.all([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]); finish(results);",
        &environment,
    )
    .expect("Promise.all should link");
    let error = futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &FirstSettledRejectionHost,
    ))
    .expect_err("a rejected aggregate fails the program");
    let rendered = error.to_string();
    assert!(
        rendered.contains("early-B"),
        "Promise.all must surface the rejection the host consumed first: {rendered}"
    );
    assert!(
        !rendered.contains("late-A"),
        "Promise.all must not surface the later rejection: {rendered}"
    );
}

/// `Promise.allSettled` is specified to preserve *input* order regardless of
/// when each leaf settled: the host answers it with every result, in input
/// order.
#[test]
fn promise_all_settled_stays_input_ordered_under_out_of_order_settlement() {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(
        "finish(await Promise.allSettled([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]));",
        &environment,
    )
    .expect("Promise.allSettled should link");
    let outcome = futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &FirstSettledRejectionHost,
    ))
    .expect("allSettled reports rejections as records");
    let ExecutionOutcome::Finished(Value::List(items)) = outcome else {
        panic!("allSettled returns a list, got {outcome:?}");
    };
    assert_eq!(items.len(), 2);
    let rendered = format!("{items:?}");
    let late = rendered.find("late-A").expect("leaf 0's reason is present");
    let early = rendered
        .find("early-B")
        .expect("leaf 1's reason is present");
    assert!(
        late < early,
        "allSettled keeps input order even when leaf 1 settled first: {rendered}"
    );
}

/// A host whose reply does not fit the aggregate that asked — a selected leaf
/// that does not exist, or a shape the consumer mode cannot produce — must be
/// refused, never repaired into a plausible answer (ADR 0099 §10 L2).
struct MisfitReplyHost;

impl ExecutionHost for MisfitReplyHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityOutcome::ResourceOperationBatch(
                match batch.consumer {
                    // Two leaves, but the host names a fifth.
                    lashlang::AggregateConsumer::All => ResourceOperationBatchOutcome::Selected {
                        leaf: 5,
                        result: ResourceOperationOutcome::Error(ExecutionHostError::new("boom")),
                    },
                    // `race` is decided by one settlement, never by every one.
                    _ => ResourceOperationBatchOutcome::AllResults(vec![
                        ResourceOperationOutcome::Value(Value::Null);
                        batch.leaves.len()
                    ]),
                },
            )),
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected malformed ability")),
        }
    }
}

#[test]
fn a_reply_that_does_not_fit_its_aggregate_fails_closed() {
    let environment = two_leaf_web_environment();
    for (source, expected) in [
        (
            "const results = await Promise.all([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]); finish(results);",
            "selected leaf 5 is out of range for 2 leaves",
        ),
        (
            "finish(await Promise.race([web.fetch({ url: 'a' }), web.fetch({ url: 'b' })]));",
            "cannot be answered with every result",
        ),
    ] {
        let linked = lash_typescript::link(source, &environment).expect("the aggregate links");
        let error = futures::executor::block_on(lashlang::execute(
            &lashlang::testing::harness::compile_linked_main(&linked),
            &mut State::new(),
            &MisfitReplyHost,
        ))
        .expect_err("a reply that does not fit its aggregate is refused");
        let rendered = error.to_string();
        assert!(
            rendered.contains(expected),
            "the refusal names what did not fit: {rendered}"
        );
    }
}

/// The canonical agent loop must compile.
///
/// The body filter used to reject every call and every member assignment, so
/// the most common loop in the language was a link-time rejection. Suspension
/// inside `for…of` is durable — the review proved it resumes across a
/// continuation round-trip — so the restriction was a conservative static
/// filter, not a durability requirement.
#[test]
fn for_of_bodies_accept_effects_and_unrelated_assignment() {
    let environment = two_leaf_web_environment();
    for source in [
        // The canonical agent loop.
        "const urls = ['a', 'b']; let out = ''; for (const url of urls) { const page = await web.fetch({ url }); out = out + page; } finish(out);",
        // A bare await on a tool, with no assignment at all.
        "const xs = ['a']; for (const x of xs) { await web.fetch({ url: x }); } finish('done');",
        // Assignment to something that is demonstrably not the iterable.
        "const xs = [1, 2]; const acc = { n: 0 }; for (const x of xs) { acc.n = acc.n + x; } finish(`${acc.n}`);",
        // A second array, written while iterating the first.
        "const xs = [1, 2]; const out = [0, 0]; for (const x of xs) { out[0] = x; } finish(`${out[0]}`);",
        // An aggregate inside the body.
        "const xs = ['a']; for (const x of xs) { await Promise.all([web.fetch({ url: x })]); } finish('done');",
        // The iterable is a call result, so nothing in the body can name it.
        "for (const c of 'ab'.concat('c')) { const page = await web.fetch({ url: c }); } finish('done');",
    ] {
        lash_typescript::link(source, &environment)
            .unwrap_or_else(|error| panic!("must link: {error}\n  source: {source}"));
    }
}

/// `for...of` follows its iterable live, as ECMA-262's iterators do
/// (FIG-3625): what the body appends, removes or replaces, through the
/// iterable's own name, an alias made before or inside the loop, a function,
/// a pattern default or a shadowing binding, is what the loop visits next.
/// Every shape the retired snapshot check refused runs to Node's answer.
#[test]
fn for_of_follows_its_iterable_live() {
    for (source, node) in [
        (
            "const xs = [1, 2]; const seen = []; for (const x of xs) { if (xs.length < 4) xs.push(x * 10); seen.push(x); } finish(seen.join(','));",
            "1,2,10,20",
        ),
        (
            "const xs = [1, 2, 3]; const seen = []; for (const x of xs) { xs.pop(); seen.push(x); } finish(seen.join(','));",
            "1,2",
        ),
        (
            "const xs = [1, 2, 3]; const seen = []; for (const x of xs) { xs[1] = 9; seen.push(x); } finish(seen.join(','));",
            "1,9,3",
        ),
        (
            "const xs = [1, 2, 3]; const seen = []; for (const x of xs) { if (x === 1) xs.splice(0, 1); seen.push(x); } finish(seen.join(','));",
            "1,3",
        ),
        (
            "const items = [1]; const same = items; for (const item of items) { if (same.length < 3) same.push(item + 1); } finish(JSON.stringify(same));",
            "[1,2,3]",
        ),
        (
            "const items = [1, 2]; const out = []; for (const item of items) { const items = [item]; out.push(items.length); } finish(out.join(','));",
            "1,1",
        ),
        (
            "const urls = ['a', 'b', 'c']; let out = ''; for (const u of urls) { const alias = urls; alias[1] = 'MUT'; out = out + u; } finish(out);",
            "aMUTc",
        ),
        (
            "const urls = ['a', 'b']; let out = ''; for (const u of urls) { const box = { inner: urls }; if (box.inner.length < 3) box.inner.push('c'); out = out + u; } finish(out);",
            "abc",
        ),
        (
            "const data = { items: ['a', 'b'] }; let out = ''; for (const u of data.items) { if (out.length > 3) break; data.items.push('z'); out = out + u; } finish(out);",
            "abzz",
        ),
        (
            "function grow(a) { if (a.length < 3) a.push(a.length); } const xs = [0]; const seen = []; for (const x of xs) { grow(xs); seen.push(x); } finish(seen.join(','));",
            "0,1,2",
        ),
        (
            "const urls = ['a', 'b']; const out = []; for (const u of urls) { const [a = urls.pop()] = []; out.push(u + a); } finish(out.join(','));",
            "ab",
        ),
        (
            "const xs = [1, 2, 3]; const seen = []; for (const x of xs) { if (x === 2) continue; if (xs.length < 5) xs.push(x + 10); seen.push(x); } finish(seen.join(','));",
            "1,3,11,13",
        ),
        (
            "const m = new Map([['a', 1]]); const seen = []; for (const [k, v] of m) { seen.push(k + v); if (k === 'a') { m.set('b', 2); m.set('a', 5); } } finish(seen.join(','));",
            "a1,b2",
        ),
        (
            "const m = new Map([['a', 1], ['b', 2], ['c', 3]]); const seen = []; for (const [k] of m) { seen.push(k); if (k === 'a') m.delete('b'); } finish(seen.join(','));",
            "a,c",
        ),
        (
            "const m = new Map([['a', 1], ['b', 2]]); const seen = []; for (const [k, v] of m) { seen.push(k + v); if (k === 'a') m.set('b', 20); } finish(seen.join(','));",
            "a1,b20",
        ),
        (
            "const m = new Map([['a', 1], ['b', 2]]); const seen = []; for (const [k] of m) { seen.push(k); if (k === 'a') m.clear(); } finish(seen.join(','));",
            "a",
        ),
        (
            "const m = new Map([['a', 1], ['b', 2]]); const seen = []; for (const [k] of m) { seen.push(k); if (k === 'a') { m.delete('a'); m.set('a', 9); } if (seen.length > 4) break; } finish(seen.join(','));",
            "a,b,a,a,a",
        ),
        (
            "const s = new Set([1]); const seen = []; for (const v of s) { seen.push(v); if (v < 3) s.add(v + 1); } finish(seen.join(','));",
            "1,2,3",
        ),
        (
            "const s = new Set([1, 2, 3]); const seen = []; for (const v of s) { seen.push(v); if (v === 1) { s.delete(2); s.add(2); } } finish(seen.join(','));",
            "1,3,2",
        ),
        (
            "const s = new Set([1, 2]); const seen = []; for (const v of s) { seen.push(v); s.add(1); } finish(seen.join(','));",
            "1,2",
        ),
        (
            "const p = new URLSearchParams('a=1&b=2'); const seen = []; for (const [k, v] of p) { seen.push(k + v); if (k === 'a') p.append('c', '3'); } finish(seen.join(','));",
            "a1,b2,c3",
        ),
        (
            "const p = new URLSearchParams('a=1&b=2&c=3'); const seen = []; for (const [k] of p) { seen.push(k); if (k === 'a') p.delete('a'); } finish(seen.join(','));",
            "a,c",
        ),
        (
            "let out = ''; for (const c of 'a😀b') { out = out + c + '|'; } finish(out);",
            "a|😀|b|",
        ),
        (
            "const xs = [[1], [2]]; const seen = []; for (const [x] of xs) { if (xs.length < 3) xs.push([x + 2]); seen.push(x); } finish(seen.join(','));",
            "1,2,3",
        ),
    ] {
        assert_eq!(
            run_typescript(source),
            Value::String(node.into()),
            "{source}"
        );
    }
}

fn run_typescript(source: &str) -> Value {
    let environment = two_leaf_web_environment();
    let linked = lash_typescript::link(source, &environment)
        .unwrap_or_else(|error| panic!("link `{source}`: {error}"));
    match futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &AggregateHost,
    ))
    .unwrap_or_else(|error| panic!("execute `{source}`: {error}"))
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

/// The overflow rewrite must not touch anything but the overflowing numbers.
///
/// It copied source bytes with `byte as char`, so every UTF-8 continuation byte
/// was reinterpreted as Latin-1 and re-encoded: one out-of-range number
/// mojibaked every non-ASCII character in the document. That replaced a typed
/// failure with silently wrong data, on exactly the host-data path the clamping
/// was justified by.
#[test]
fn json_overflow_rewriting_preserves_non_ascii_text() {
    for (source, expected) in [
        (r#"finish(JSON.parse('{"a":"café","n":1e400}').a);"#, "café"),
        (
            r#"finish(JSON.parse('{"a":"日本語","n":1e400}').a);"#,
            "日本語",
        ),
        (
            r#"finish(JSON.parse('{"a":"emoji 😀 tail","n":1e400}').a);"#,
            "emoji 😀 tail",
        ),
        // Multi-byte characters immediately either side of the rewritten token.
        (
            r#"finish(JSON.parse('{"a":"é","n":1e400,"b":"ü"}').b);"#,
            "ü",
        ),
        // No overflow at all: the untouched path must stay correct too.
        (r#"finish(JSON.parse('{"a":"café"}').a);"#, "café"),
    ] {
        assert_eq!(
            run_typescript(source),
            Value::String(expected.into()),
            "{source}"
        );
    }
}

/// Guest data shaped like the rewrite's own marker must never be reinterpreted.
#[test]
fn json_overflow_sentinel_does_not_collide_with_guest_data() {
    let value = run_typescript(
        r#"finish(JSON.stringify(JSON.parse('{"o":{"__lash_json_f64_overflow_sign__":1},"n":1e400}').o));"#,
    );
    assert_eq!(
        value,
        Value::String(r#"{"__lash_json_f64_overflow_sign__":1}"#.into()),
        "a guest object that looks like the marker survives unchanged"
    );
}

/// `map` must actually run: it was advertised, accepted, and then failed at
/// run time with a host-boundary error because the stdlib builtin exports
/// every argument across the boundary and a closure cannot cross it.
#[test]
fn array_map_runs_its_callback_in_the_vm() {
    assert_eq!(
        run_typescript("finish([1,2,3].map((x) => x * 2).join('-'));"),
        Value::String("2-4-6".into())
    );
    assert_eq!(
        run_typescript("finish(['a','b'].map((x, i) => x + i).join('-'));"),
        Value::String("a0-b1".into())
    );
    assert_eq!(
        run_typescript("const a = [1,2,3]; finish(a.map((x) => x + 1).join('-'));"),
        Value::String("2-3-4".into())
    );
    // The callback closes over its environment.
    assert_eq!(
        run_typescript("const k = 10; finish([1,2].map((x) => x + k).join('-'));"),
        Value::String("11-12".into())
    );
    assert_eq!(
        run_typescript("finish([].map((x) => x).join('-'));"),
        Value::String("".into())
    );
}

/// Callback arity is ordinary ECMAScript call arity: named functions and
/// three-argument callbacks are valid, while a missing callback rejects.
#[test]
fn array_map_accepts_ecma_callback_shapes_and_rejects_a_missing_callback() {
    assert_eq!(
        run_typescript(
            "function d(x: number): number { return x * 2; } finish([1,2].map(d).join('-'));"
        ),
        Value::String("2-4".into())
    );
    assert_eq!(
        run_typescript("finish([1,2].map((x, i, all) => x+i+all.length).join('-'));"),
        Value::String("3-5".into())
    );
    for source in [
        "finish([1,2].map().join('-'));",
        "finish([1,2].filter().join('-'));",
        "finish([1,2].reduce());",
    ] {
        let environment = two_leaf_web_environment();
        let error = lash_typescript::link(source, &environment)
            .expect_err("a missing callback must reject at link time");
        assert_eq!(error.code.as_str(), "TS_METHOD_UNSUPPORTED", "{source}");
    }
}

/// The register states that a `map` callback cannot perform effects and that
/// there is therefore no suspension point inside `map` to make durable. Both
/// halves are claims about behaviour, so both are pinned here rather than
/// asserted in prose alone.
#[test]
fn map_callbacks_cannot_perform_effects() {
    let environment = two_leaf_web_environment();

    // An effect inside the callback terminates with the typed error the
    // register names — not an untyped host failure, and not silently working.
    let linked = lash_typescript::link(
        "const out = [1,2].map((x) => { console.log(x); return x; }); finish(out);",
        &environment,
    )
    .expect("an effect inside a callback is not a link-time rejection today");
    let error = futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &AggregateHost,
    ))
    .expect_err("an effect inside a map callback must fail");
    assert!(
        error
            .to_string()
            .contains("effects are not supported inside builtin callbacks"),
        "the failure is the typed builtin-callback error: {error}"
    );

    // `await` inside the callback never reaches run time, so no suspension can
    // occur inside `map`.
    let rejected = lash_typescript::link(
        "const out = [1,2].map(async (x) => { return await fetchPage('a'); }); finish(out);",
        &environment,
    )
    .expect_err("an async callback must reject");
    assert!(
        rejected.code.as_str().starts_with("TS_"),
        "the rejection is named: {rejected}"
    );
}

#[test]
fn runtime_array_rejections_report_the_selected_rejection() {
    let environment = two_leaf_web_environment();
    for array in [
        "['a', 'b'].map(url => web.fetch({url}))",
        "[web.fetch({url:'a'}), 42, web.fetch({url:'b'})]",
    ] {
        let source = format!("const pending = {array}; finish(await Promise.all(pending));");
        let linked = lash_typescript::link(&source, &environment).expect("runtime array links");
        let error = futures::executor::block_on(lashlang::execute(
            &lashlang::testing::harness::compile_linked_main(&linked),
            &mut State::new(),
            &FirstSettledRejectionHost,
        ))
        .expect_err("both leaves reject");
        assert!(error.to_string().contains("early-B"), "{source}: {error}");
    }
}

/// Every kind of aggregate leaf crosses a durable park.
///
/// The aggregate mixes a pending tool handle with a plain value, and the park
/// lands between minting them and settling them. Under ADR 0087 a third kind
/// rode along — a process handle settled in a second phase — and the
/// continuation had to carry that encoding too. There is one handle kind and
/// one settlement phase now, so what has to survive the park is the pending
/// request map and the values beside it.
#[test]
fn pending_tool_handles_survive_durable_process_park() {
    for mode in ["all", "allSettled"] {
        let source = format!(
            r#"const worker = async () => {{
            const pending = [web.fetch({{value: "kept"}}), 42];
            await sleep(5);
            return await Promise.{mode}(pending);
        }};
        finish(await processes.start({{ definition: worker }}));"#
        );
        let expected = if mode == "all" {
            serde_json::json!(["kept", 42])
        } else {
            serde_json::json!([
                {"status":"fulfilled","value":"kept"},
                {"status":"fulfilled","value":42}
            ])
        };
        assert_eq!(
            suspend_and_resume_process(&source, serde_json::json!({}), 0),
            ExecutionOutcome::Finished(lashlang::from_json(expected)),
            "{mode}"
        );
    }
}

/// The host an aggregate over tool leaves and process handles runs against.
///
/// Under ADR 0087 it answered two settlement phases. There is one recorded
/// batch order now (ADR 0095), so its `Await` arm is reached only by the typed
/// direct `await handle`, never by an aggregate: a handle written at an element
/// position is refused before the host is asked anything.
struct MixedAggregateHost;

impl MixedAggregateHost {
    fn settle(operation: &lashlang::ResourceOperation) -> ResourceOperationOutcome {
        let args = operation.args.first().and_then(Value::as_record);
        match args.and_then(|record| record.get("fail")) {
            Some(Value::Bool(true)) => {
                ResourceOperationOutcome::Error(ExecutionHostError::new("tool failed"))
            }
            _ => ResourceOperationOutcome::Value(
                args.and_then(|record| record.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null),
            ),
        }
    }
}

impl ExecutionHost for MixedAggregateHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityOutcome::ResourceOperationBatch(
                batch.answer_in_leaf_order(
                    batch
                        .leaves
                        .iter()
                        .filter_map(lashlang::ResourceOperationBatchLeaf::operation)
                        .map(Self::settle)
                        .collect(),
                ),
            )),
            AbilityOp::ResourceOperation(call) if call.operation == "start" => {
                // The start's own arguments ride in `args`, beside the
                // `definition` slot that carries the process itself.
                let input = call
                    .args
                    .first()
                    .and_then(Value::as_record)
                    .and_then(|record| record.get("args"))
                    .and_then(Value::as_record)
                    .and_then(|record| record.get("input"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(AbilityOutcome::Value(lashlang::from_json(
                    process_handle_json(&input.to_string()),
                )))
            }
            AbilityOp::Await(handle) => {
                let id = handle
                    .as_record()
                    .and_then(|record| record.get("process_id"))
                    .map(Value::to_string)
                    .unwrap_or_default();
                if id == lash_sansio::ProcessId::fixture("fail-p").as_str() {
                    Err(ExecutionHostError::new("process fail-p failed"))
                } else {
                    Ok(AbilityOutcome::Value(Value::String(
                        format!("process {id} done").into(),
                    )))
                }
            }
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unexpected mixed-aggregate ability",
            )),
        }
    }
}

fn run_mixed_aggregate(body: &str) -> Result<ExecutionOutcome, lashlang::RuntimeError> {
    let source = format!(
        r#"const worker = async (input: unknown) => input;
        {body}"#
    );
    let linked = lash_typescript::link(&source, &mixed_aggregate_environment())
        .expect("mixed aggregate should link");
    futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &MixedAggregateHost,
    ))
}

/// A process handle written at an element position of an aggregate is refused,
/// in both aggregate forms and both written positions.
///
/// This replaces three ADR 0087 laws at once — a tool rejection beat a process
/// failure wherever written, a process failure surfaced only once every tool
/// leaf succeeded, and `allSettled` reported a process outcome in array order.
/// All three described the ordering between two settlement phases. There is one
/// phase now: an aggregate is one resource-operation batch whose recorded order
/// is authoritative, and a durable wait joins it as `processes.await(handle)`,
/// the leaf tool that parks on it. Which rejection that one order reports is
/// pinned in `lash-core`'s `session::settlement_latency_tests`.
#[test]
fn a_process_handle_is_not_an_aggregate_leaf() {
    for method in ["all", "allSettled"] {
        for body in [
            format!(
                "const h = await processes.start({{ definition: worker, args: {{ input: 'fail-p' }} }}); \
                 finish(await Promise.{method}([web.fetch({{ fail: true }}), h]));"
            ),
            format!(
                "const h = await processes.start({{ definition: worker, args: {{ input: 'fail-p' }} }}); \
                 finish(await Promise.{method}([h, web.fetch({{ fail: true }})]));"
            ),
        ] {
            let error = run_mixed_aggregate(&body).expect_err(&body);
            let rendered = error.to_string();
            assert!(
                rendered.contains("processes.await(handle)"),
                "{body}: the refusal must name the tool that parks on the wait: {rendered}"
            );
            assert!(
                !rendered.contains("process fail-p failed"),
                "{body}: the process seam is never reached: {rendered}"
            );
        }
    }
}

#[test]
fn promise_all_keeps_nested_process_handles_shallow() {
    let body = "const h = await processes.start({ definition: worker, args: { input: 'p' } }); \
                finish(await Promise.all([[h], web.fetch({ value: 1 })]));";
    assert_eq!(
        run_mixed_aggregate(body).expect("nested process handle remains an ordinary value"),
        ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!([
            [process_handle_json("p")],
            1
        ])))
    );
}

/// A tool handle is an execution-scoped identity: a settled handle a cell left
/// in a root binding is not exported to the session, and a handle record that
/// does arrive from an earlier execution is refused rather than aliased onto
/// the current execution's first request.
#[test]
fn tool_handles_do_not_cross_cells() {
    let environment = two_leaf_web_environment();
    let mut state = State::new();
    let linked = lash_typescript::link(
        "const kept = 5; const p = web.fetch({ value: 1 }); await p; finish(kept);",
        &environment,
    )
    .expect("first cell should link");
    futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut state,
        &MixedAggregateHost,
    ))
    .expect("first cell should finish");
    assert_eq!(state.globals().get("kept"), Some(&Value::Number(5.0)));
    assert!(
        state.globals().get("p").is_none(),
        "a tool handle must not be exported as a session global: {:?}",
        state.globals().get("p")
    );

    let stale = lashlang::from_json(serde_json::json!({
        // The forgery a cell could most plausibly attempt: the one handle
        // shape, spelled with the nonce an execution that allocated nothing
        // would mint. It still names no live request (ADR 0095).
        "p": { "__handle__": "lash", "id": "t.0000000000000000.0" }
    }));
    let mut state = State::from_snapshot(lashlang::Snapshot::new(
        stale.as_record().expect("globals record").clone(),
    ));
    let linked = lash_typescript::link(
        "const q = web.fetch({ value: 2 }); q; finish(await p);",
        &environment.with_globals(["p"]),
    )
    .expect("second cell should link");
    let error = futures::executor::block_on(lashlang::execute(
        &lashlang::testing::harness::compile_linked_main(&linked),
        &mut state,
        &MixedAggregateHost,
    ))
    .expect_err("a stale handle must not alias the new request");
    let lashlang::RuntimeError::PendingTool { problem, .. } = &error else {
        panic!("expected the typed pending-tool refusal: {error}");
    };
    assert!(
        problem.contains("not minted by this execution"),
        "{problem}"
    );
}

#[path = "agent_surface/adr_claims.rs"]
mod adr_claims;
