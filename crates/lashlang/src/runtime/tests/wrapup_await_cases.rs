use super::*;

/// Records every host ability a comprehension-await program performs: one
/// entry per batch (its operations, in order) and a count of single calls.
#[derive(Default)]
struct ComprehensionBatchHost {
    batches: Mutex<Vec<Vec<String>>>,
    singles: AtomicUsize,
}

impl ComprehensionBatchHost {
    fn perform_operation(operation: ResourceOperation) -> Result<Value, ExecutionHostError> {
        match operation.operation.as_str() {
            "order" => {
                let id = operation
                    .args
                    .first()
                    .and_then(Value::as_record)
                    .and_then(|record| record.get("id"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let mut record = Record::new();
                record.insert("id".to_string(), id);
                record.insert("status".to_string(), Value::String("shipped".into()));
                Ok(Value::Record(Arc::new(record)))
            }
            "err" => Err(ExecutionHostError::new(format!(
                "boom {}",
                Self::describe(&operation)
            ))),
            "maybe_err" => {
                if Self::describe(&operation) == "maybe_err:b" {
                    Err(ExecutionHostError::new("failed order b"))
                } else {
                    Ok(Value::Null)
                }
            }
            _ => Host::perform_resource_operation(operation),
        }
    }

    fn describe(operation: &ResourceOperation) -> String {
        let arg = operation
            .args
            .first()
            .and_then(Value::as_record)
            .and_then(|record| record.get("value").or_else(|| record.get("id")))
            .map(|value| value.to_string())
            .unwrap_or_default();
        format!("{}:{arg}", operation.operation)
    }

    fn batches(&self) -> Vec<Vec<String>> {
        self.batches.lock_recover().clone()
    }
}

impl ExecutionHost for ComprehensionBatchHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                self.singles.fetch_add(1, Ordering::SeqCst);
                Self::perform_operation(operation).map(AbilityResult::Value)
            }
            AbilityOp::ResourceOperationBatch(batch) => {
                self.batches
                    .lock_recover()
                    .push(batch.operations.iter().map(Self::describe).collect());
                // Deliberately settle in reverse order, so a test can tell
                // settlement order from written order.
                let mut results =
                    vec![ResourceOperationResult::Value(Value::Null); batch.operations.len()];
                let mut settlement_order = Vec::with_capacity(results.len());
                for (index, operation) in batch.operations.into_iter().enumerate().rev() {
                    results[index] =
                        ResourceOperationResult::from_result(Self::perform_operation(operation));
                    settlement_order.push(index);
                }
                Ok(AbilityResult::ResourceOperationBatch(
                    ResourceOperationBatchResult {
                        results,
                        settlement_order,
                    },
                ))
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected host ability in comprehension await test: {other:?}"
            ))),
        }
    }
}

#[derive(Default)]
struct AggregateProcessHost {
    awaits: AtomicUsize,
    reject_await: bool,
}

impl ExecutionHost for AggregateProcessHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult::settled_in_input_order(
                    batch
                        .operations
                        .iter()
                        .map(|operation| {
                            if operation.operation == "err" {
                                ResourceOperationResult::Error(ExecutionHostError::new(
                                    "tool failed",
                                ))
                            } else {
                                ResourceOperationResult::Value(Value::Number(7.0))
                            }
                        })
                        .collect(),
                ),
            )),
            AbilityOp::StartProcess(_) => {
                let mut handle = Record::new();
                handle.insert(
                    lash_sansio::handle::HANDLE_FIELD.to_string(),
                    Value::String(lash_sansio::handle::HANDLE_KIND.into()),
                );
                handle.insert(
                    "id".to_string(),
                    Value::String(
                        lash_sansio::handle::HandleId::process("h", 1)
                            .as_str()
                            .into(),
                    ),
                );
                Ok(AbilityResult::Value(Value::Record(Arc::new(handle))))
            }
            AbilityOp::Await(_) => {
                self.awaits.fetch_add(1, Ordering::SeqCst);
                if self.reject_await {
                    Err(ExecutionHostError::new("process failed"))
                } else {
                    Ok(AbilityResult::Value(Value::Number(42.0)))
                }
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            other => Err(ExecutionHostError::new(format!(
                "unexpected host ability in aggregate process await test: {other:?}"
            ))),
        }
    }
}

/// Builds the module every case in this file links against.
///
/// `echo` is the process the aggregate cases start; the module operations are
/// the leaves they settle. Callers pass the top-level expressions, so the
/// declaration list stays in one place.
fn comprehension_module(expressions: Vec<Expr>) -> Program {
    builders::module(
        vec![builders::process(
            "echo",
            Vec::new(),
            builders::finish(builders::null()),
        )],
        expressions,
    )
}

/// `h = start echo()` — the handle the aggregate cases await.
fn started_handle() -> Expr {
    builders::assign("h", builders::start("echo", Vec::new()))
}

/// `<module>.<operation>({ <args> })?` — an unawaited leaf.
///
/// Every use here sits inside an awaited aggregate, which is what settles it;
/// an `await` on the leaf itself would be a different program.
fn op(module: &str, operation: &str, args: Vec<(&str, Expr)>) -> Expr {
    builders::unwrap(builders::receiver_call(
        builders::resource(&[module]),
        operation,
        vec![builders::record(args)],
    ))
}

fn comprehension_compile(program: Program) -> CompiledProgram {
    let mut catalog = crate::LashlangHostCatalog::new();
    for (module, operation) in [
        ("tools", "echo"),
        ("tools", "err"),
        ("tools", "maybe_err"),
        ("retail", "order"),
    ] {
        catalog
            .add_module_operation(
                [module],
                module,
                operation,
                operation,
                crate::TypeExpr::Any,
                crate::TypeExpr::Any,
            )
            .unwrap();
    }
    let linked = crate::LinkedModule::link(
        program,
        crate::LashlangHostEnvironment::new(catalog, crate::LashlangAbilities::all()),
    )
    .expect("program should link");
    crate::compile_linked(&linked)
}

async fn comprehension_finish(host: &ComprehensionBatchHost, program: Program) -> Value {
    let compiled = comprehension_compile(program);
    let mut state = State::new();
    match execute_compiled(&compiled, &mut state, host)
        .await
        .expect("program should run")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

async fn aggregate_process_finish(host: &AggregateProcessHost, program: Program) -> Value {
    let compiled = comprehension_compile(program);
    let mut state = State::new();
    match execute_compiled(&compiled, &mut state, host)
        .await
        .expect("program should run")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

/// A handle written at an element position of an awaited literal, beside a
/// module leaf, is refused.
///
/// ADR 0087 settled such a handle through the process-await seam in a second
/// phase after the tool batch. There is one recorded batch order now, so the
/// handle is not settled at all: the repair names `processes.await(handle)`,
/// the tool that parks on the durable wait and therefore is a leaf of that one
/// batch. The seam is never reached.
#[tokio::test(flavor = "current_thread")]
async fn a_literal_process_handle_element_is_refused_before_the_process_seam() {
    let host = AggregateProcessHost::default();
    let compiled = comprehension_compile(comprehension_module(vec![
        started_handle(),
        builders::finish(builders::await_expr(builders::list(vec![
            builders::var("h"),
            op("tools", "echo", Vec::new()),
        ]))),
    ]));
    let error = execute_compiled(&compiled, &mut State::new(), &host)
        .await
        .expect_err("a process handle is not an aggregate leaf");
    assert!(
        error.to_string().contains("processes.await(handle)"),
        "{error}"
    );
    assert_eq!(host.awaits.load(Ordering::SeqCst), 0);
}

/// A handle nested *inside* an element is not at an element position, so it is
/// carried through as data rather than refused — and no batch settles it
/// either. ADR 0087's recursive process-leaf walk reached it; ADR 0096's
/// shallow element semantics, which the single batch order inherits, do not.
#[tokio::test(flavor = "current_thread")]
async fn a_handle_nested_inside_a_literal_element_is_carried_through() {
    let host = AggregateProcessHost::default();
    let value = aggregate_process_finish(
        &host,
        comprehension_module(vec![
            started_handle(),
            builders::finish(builders::await_expr(builders::list(vec![
                builders::list(vec![builders::var("h")]),
                op("tools", "echo", Vec::new()),
            ]))),
        ]),
    )
    .await;
    assert_eq!(
        value.to_string(),
        r#"[[{"__handle__":"lash","id":"p.1.h"}],7]"#
    );
    assert_eq!(host.awaits.load(Ordering::SeqCst), 0);
}

/// A container bound to a name is a plain value in an awaited aggregate: only
/// element positions settle, so the handles inside it are carried through
/// untouched. This is Promise's shallow element semantics, the only ones this
/// runtime has (ADR 0096); the recursive walk it replaces was the retired
/// surface dialect's.
#[tokio::test(flavor = "current_thread")]
async fn bound_process_containers_are_carried_through_unsettled() {
    // Each case binds a container to `hs`/`hr`, then awaits a literal whose
    // first element is that bound name: only element positions settle.
    let awaited = |bound: &str, container: Expr| {
        comprehension_module(vec![
            started_handle(),
            builders::assign(bound, container),
            builders::finish(builders::await_expr(builders::list(vec![
                builders::var(bound),
                op("tools", "echo", Vec::new()),
            ]))),
        ])
    };
    for (label, program, expected) in [
        (
            "list of one handle",
            awaited("hs", builders::list(vec![builders::var("h")])),
            r#"[[{"__handle__":"lash","id":"p.1.h"}],7]"#,
        ),
        (
            "record holding a handle",
            awaited("hr", builders::record(vec![("child", builders::var("h"))])),
            r#"[{"child":{"__handle__":"lash","id":"p.1.h"}},7]"#,
        ),
        (
            "handle nested three lists deep",
            awaited(
                "hs",
                builders::list(vec![builders::list(vec![builders::list(vec![
                    builders::var("h"),
                ])])]),
            ),
            r#"[[[[{"__handle__":"lash","id":"p.1.h"}]]],7]"#,
        ),
        (
            "handle beside plain values",
            awaited(
                "hs",
                builders::list(vec![
                    builders::num(1.0),
                    builders::var("h"),
                    builders::record(vec![("note", builders::string("kept"))]),
                ]),
            ),
            r#"[[1,{"__handle__":"lash","id":"p.1.h"},{"note":"kept"}],7]"#,
        ),
    ] {
        let host = AggregateProcessHost::default();
        let value = aggregate_process_finish(&host, program).await;
        assert_eq!(value.to_string(), expected, "{label}");
        assert_eq!(host.awaits.load(Ordering::SeqCst), 0, "{label}");
    }
}

/// A carried handle is data, so a rejecting leaf rejects the aggregate exactly
/// as it would with any other value beside it, and the process seam is never
/// reached. Under ADR 0087 this passed for a different reason: the tool batch
/// was phase one and its rejection ended the await before phase two ran.
#[tokio::test(flavor = "current_thread")]
async fn a_rejecting_leaf_rejects_the_aggregate_beside_a_carried_handle() {
    let host = AggregateProcessHost {
        reject_await: true,
        ..AggregateProcessHost::default()
    };
    let compiled = comprehension_compile(comprehension_module(vec![
        started_handle(),
        builders::assign("hs", builders::list(vec![builders::var("h")])),
        builders::finish(builders::await_expr(builders::list(vec![
            builders::var("hs"),
            op("tools", "err", Vec::new()),
        ]))),
    ]));
    let error = execute_compiled(&compiled, &mut State::new(), &host)
        .await
        .expect_err("the module rejection must win");
    assert!(error.to_string().contains("tool failed"), "{error}");
    assert_eq!(host.awaits.load(Ordering::SeqCst), 0);
}

#[test]
fn awaiting_a_settled_literal_is_a_link_diagnostic() {
    let pair = || builders::list(vec![builders::num(1.0), builders::num(2.0)]);
    let sum = || builders::binary(builders::num(1.0), BinaryOp::Add, builders::num(2.0));
    for (source, awaited, kind) in [
        ("finish await [1, 2]", pair(), "list"),
        (
            "finish await { a: 1 }",
            builders::record(vec![("a", builders::num(1.0))]),
            "record",
        ),
        (
            "finish await [x for x in [1, 2]]",
            builders::comprehension(
                builders::var("x"),
                vec![builders::comprehension_for("x", pair())],
            ),
            "list",
        ),
        ("finish await 1", builders::num(1.0), "number"),
        ("finish await (1 + 2)", sum(), "number"),
        ("finish await [1 + 2]", builders::list(vec![sum()]), "list"),
        (
            "finish await { a: 1 + 2 }",
            builders::record(vec![("a", sum())]),
            "record",
        ),
    ] {
        let program = builders::program(vec![builders::finish(builders::await_expr(awaited))]);
        let diagnostic = link_diagnostic(program, source);
        assert!(
            diagnostic.contains(&format!("`await` of a settled {kind}:")),
            "{source}: {diagnostic}"
        );
        assert!(
            diagnostic.contains("await [m.op({ id: x })? for x in xs]"),
            "{source}: {diagnostic}"
        );
    }
}

#[test]
fn awaiting_the_handle_record_shape_is_not_rejected_as_settled() {
    let program = builders::program(vec![builders::finish(builders::await_expr(
        builders::record(vec![("__handle__", builders::string("h"))]),
    ))]);
    crate::LinkedModule::link(program, runtime_test_environment())
        .expect("handle shape should link");
}

#[test]
fn a_bare_handle_field_is_an_ordinary_record_and_awaiting_it_is_visibly_settled() {
    // Before ADR 0095 the loose reader accepted a bare `handle` key as a
    // handle, which is how a plain record could reach the process-await path.
    // There is one handle kind now, and it is marked by `__handle__` alone.
    let program = builders::program(vec![builders::finish(builders::await_expr(
        builders::record(vec![("handle", builders::string("h"))]),
    ))]);
    let error = crate::LinkedModule::link(program, runtime_test_environment())
        .expect_err("a bare `handle` field is not a handle");
    assert!(
        format!("{error:?}").contains("record"),
        "expected a visibly-settled record diagnostic, got: {error:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_comprehension_aggregates_match_literal_expansion() {
    // `echo_for(x)` is the leaf a comprehension produces per element; the
    // literal column spells the same leaves out by hand.
    let echo_of = |value: Expr| op("tools", "echo", vec![("value", value)]);
    let echo_num = |value: f64| echo_of(builders::num(value));
    // `[m.op({ value: x })? for x in <items>]`
    let comp_over = |items: Expr| {
        builders::comprehension(
            echo_of(builders::var("x")),
            vec![builders::comprehension_for("x", items)],
        )
    };
    let one_two = || builders::list(vec![builders::num(1.0), builders::num(2.0)]);
    let finish_await =
        |expr: Expr| comprehension_module(vec![builders::finish(builders::await_expr(expr))]);

    for (label, nested, literal) in [
        (
            "comprehension inside an awaited tuple",
            finish_await(builders::tuple(vec![
                comp_over(one_two()),
                builders::num(7.0),
            ])),
            finish_await(builders::tuple(vec![
                builders::list(vec![echo_num(1.0), echo_num(2.0)]),
                builders::num(7.0),
            ])),
        ),
        (
            "comprehension inside an awaited record field",
            finish_await(builders::record(vec![("orders", comp_over(one_two()))])),
            finish_await(builders::record(vec![(
                "orders",
                builders::list(vec![echo_num(1.0), echo_num(2.0)]),
            )])),
        ),
        (
            "comprehension nested under a list, record and tuple",
            finish_await(builders::list(vec![builders::tuple(vec![
                builders::list(vec![builders::record(vec![(
                    "orders",
                    comp_over(one_two()),
                )])]),
                echo_num(3.0),
            ])])),
            finish_await(builders::list(vec![builders::tuple(vec![
                builders::list(vec![builders::record(vec![(
                    "orders",
                    builders::list(vec![echo_num(1.0), echo_num(2.0)]),
                )])]),
                echo_num(3.0),
            ])])),
        ),
        (
            "comprehension over a bound list of rows, including an empty row",
            comprehension_module(vec![
                builders::assign(
                    "rows",
                    builders::list(vec![
                        one_two(),
                        builders::list(Vec::new()),
                        builders::list(vec![builders::num(3.0)]),
                    ]),
                ),
                builders::finish(builders::await_expr(builders::comprehension(
                    comp_over(builders::var("row")),
                    vec![builders::comprehension_for("row", builders::var("rows"))],
                ))),
            ]),
            finish_await(builders::list(vec![
                builders::list(vec![echo_num(1.0), echo_num(2.0)]),
                builders::list(Vec::new()),
                builders::list(vec![echo_num(3.0)]),
            ])),
        ),
        (
            "empty comprehension between two settled leaves",
            finish_await(builders::record(vec![
                ("before", echo_num(0.0)),
                ("orders", comp_over(builders::list(Vec::new()))),
                ("after", echo_num(3.0)),
            ])),
            finish_await(builders::record(vec![
                ("before", echo_num(0.0)),
                ("orders", builders::list(Vec::new())),
                ("after", echo_num(3.0)),
            ])),
        ),
    ] {
        let nested_host = ComprehensionBatchHost::default();
        let literal_host = ComprehensionBatchHost::default();
        let expected = comprehension_finish(&literal_host, literal).await;
        assert_eq!(
            comprehension_finish(&nested_host, nested).await,
            expected,
            "{label}"
        );
        assert_eq!(nested_host.batches(), literal_host.batches(), "{label}");
        assert_eq!(nested_host.batches().len(), 1, "{label}");
        assert_eq!(nested_host.singles.load(Ordering::SeqCst), 0, "{label}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn nested_comprehension_rejections_follow_settlement_order() {
    let host = ComprehensionBatchHost::default();
    let compiled = comprehension_compile(comprehension_module(vec![builders::finish(
        builders::await_expr(builders::record(vec![
            (
                "orders",
                builders::comprehension(
                    builders::comprehension(
                        op("tools", "err", vec![("value", builders::var("x"))]),
                        vec![builders::comprehension_for("x", builders::var("row"))],
                    ),
                    vec![builders::comprehension_for(
                        "row",
                        builders::list(vec![builders::list(vec![
                            builders::string("first"),
                            builders::string("second"),
                        ])]),
                    )],
                ),
            ),
            (
                "last",
                op("tools", "echo", vec![("value", builders::string("last"))]),
            ),
        ])),
    )]));
    let error = execute_compiled(&compiled, &mut State::new(), &host)
        .await
        .unwrap_err();
    // `Promise.all` reports the rejection that settled first (ADR 0096), and
    // this host settles in reverse: the last-written leaf wins.
    assert!(error.to_string().contains("second"), "{error}");
    assert_eq!(
        host.batches(),
        vec![vec![
            "err:first".to_string(),
            "err:second".to_string(),
            "echo:last".to_string()
        ]]
    );
    assert_eq!(host.singles.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_settled_await_names_the_value_and_nested_path() {
    let host = ComprehensionBatchHost::default();
    let compiled = comprehension_compile(comprehension_module(vec![
        builders::assign(
            "value",
            op(
                "tools",
                "echo",
                vec![(
                    "value",
                    builders::record(vec![("orders", builders::list(vec![builders::num(7.0)]))]),
                )],
            ),
        ),
        builders::finish(builders::await_expr(builders::var("value"))),
    ]));
    let error = execute_compiled(&compiled, &mut State::new(), &host)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::AwaitExpectsHandle { .. }));
    assert!(
        error.to_string().contains("number at `orders[0]`"),
        "{error}"
    );
}
