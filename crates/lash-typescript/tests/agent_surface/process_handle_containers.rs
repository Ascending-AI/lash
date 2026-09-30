use super::*;

#[derive(Default)]
struct ContainerHost {
    calls: std::sync::Mutex<Vec<String>>,
}

impl ExecutionHost for ContainerHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(call) if call.operation == "start" => {
                Ok(AbilityOutcome::Value(process_handle("container-child")))
            }
            AbilityOp::ResourceOperation(call)
                if matches!(call.operation.as_str(), "signal" | "await") =>
            {
                let [Value::Record(args)] = call.args.as_slice() else {
                    panic!("process control receives record arguments: {:?}", call.args);
                };
                assert_eq!(
                    args.get("handle"),
                    Some(&process_handle("container-child")),
                    "{} must receive the stored handle",
                    call.operation
                );
                self.calls.lock().expect("calls lock").push(call.operation);
                Ok(AbilityOutcome::Value(Value::String("joined".into())))
            }
            AbilityOp::ResourceOperation(call) if call.operation == "ping" => {
                Ok(AbilityOutcome::Value(Value::Null))
            }
            AbilityOp::Sleep(_) => Ok(AbilityOutcome::Value(Value::Null)),
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            other => panic!("unexpected container-law ability: {other:?}"),
        }
    }
}

fn environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    let handle = serde_json::json!({ "x-lash": { "kind": "process_unknown" } });
    for (operation, input, output) in [
        (
            "start",
            serde_json::json!({"type":"object", "additionalProperties":false, "properties":{"definition":handle}, "required":["definition"]}),
            handle.clone(),
        ),
        (
            "signal",
            serde_json::json!({"type":"object", "additionalProperties":false, "properties":{"handle":handle,"signal":{"type":"string"},"payload":{}}, "required":["handle"]}),
            serde_json::json!({}),
        ),
        (
            "await",
            serde_json::json!({"type":"object", "additionalProperties":false, "properties":{"handle":handle}, "required":["handle"]}),
            serde_json::json!({}),
        ),
    ] {
        catalog
            .add_module_operation_contract(
                ["processes"],
                "Processes",
                operation,
                format!("tool:processes/{operation}"),
                &lashlang::OperationContract::new(input, output),
            )
            .expect("process control operation");
    }
    catalog
        .add_module_operation_contract(
            ["tools"],
            "Tools",
            "ping",
            "tool:tools/ping",
            &lashlang::OperationContract::new(serde_json::json!({}), serde_json::json!({})),
        )
        .expect("ping operation");
    lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::all())
}

fn law(storage: &str, access: &str, suspension: &str, replay: bool) {
    let source = format!(
        r#"
        const child = async () => {{ return "joined"; }};
        {storage}
        {suspension}
        await processes.signal({{handle: {access}, signal: "ready", payload: 1}});
        const joined = await processes.await({{handle: {access}}});
        finish({{handle: {access}, joined: joined}});
    "#
    );
    let linked = lash_typescript::link(&source, &environment()).expect("container law links");
    let compiled = lashlang::testing::harness::compile_linked_main(&linked);
    let host = ContainerHost::default();
    let mut state = State::new();
    let execution_environment = lashlang::ExecutionEnvironment::new(&host).process();
    let outcomes = futures::executor::block_on(async {
        let mut vm = Vm::from_state(&compiled, &mut state, &execution_environment).expect("VM");
        let checkpoint = if replay {
            for _ in 0..2 {
                assert_eq!(
                    vm.run_process_until_effect().await.expect("effect"),
                    VmRunOutcome::EffectCompleted
                );
            }
            Some(serde_json::to_vec(&vm.suspend().expect("suspend")).expect("encode"))
        } else {
            None
        };
        let mut outcomes = Vec::new();
        for _ in 0..if replay { 2 } else { 1 } {
            if let Some(bytes) = &checkpoint {
                drop(vm);
                let continuation = lashlang::VmInstance::pristine()
                    .open_continuation(bytes)
                    .expect("decode");
                vm = Vm::resume_from(continuation, &compiled, &execution_environment)
                    .expect("resume");
            }
            let mut completed = false;
            for _ in 0..16 {
                match vm
                    .run_process_until_effect()
                    .await
                    .expect("container law executes")
                {
                    VmRunOutcome::EffectCompleted => {}
                    VmRunOutcome::Complete(outcome) => {
                        outcomes.push(outcome);
                        completed = true;
                        break;
                    }
                    VmRunOutcome::HandedOver => panic!("container host does not hand over"),
                }
            }
            assert!(completed, "the law completes within its effect bound");
        }
        outcomes
    });
    for outcome in outcomes {
        assert_eq!(
            outcome,
            ExecutionOutcome::Finished(lashlang::from_json(serde_json::json!({
                "handle": process_handle_json("container-child"), "joined": "joined"
            })))
        );
    }
    let expected_calls = if replay {
        vec!["signal", "await", "signal", "await"]
    } else {
        vec!["signal", "await"]
    };
    assert_eq!(*host.calls.lock().expect("calls lock"), expected_calls);
}

const LIST: &str = "const handles=[]; for(let i=0;i<1;i=i+1){handles.push(await processes.start({definition:child}));}";
const OBJECT: &str = "const handles={child:await processes.start({definition:child})};";

#[test]
fn list_storage_preserves_process_handles() {
    for (storage, access) in [
        (LIST, "handles[0]"),
        (
            "const handles=[]; handles[0]=await processes.start({definition:child});",
            "handles[0]",
        ),
        (
            "const handles=[null]; handles.push(await processes.start({definition:child}));",
            "handles[1]",
        ),
        (
            "const handles=[]; const alias=handles; alias.push(await processes.start({definition:child}));",
            "handles[0]",
        ),
        (
            "const box={handles:[]}; box.handles.push(await processes.start({definition:child}));",
            "box.handles[0]",
        ),
    ] {
        law(storage, access, "", false);
    }
}

#[test]
fn object_storage_preserves_process_handles() {
    law(OBJECT, "handles.child", "", false);
}

#[test]
fn list_storage_preserves_process_handles_across_suspension() {
    for suspension in ["await sleep(1);", "await tools.ping({});"] {
        law(LIST, "handles[0]", suspension, false);
    }
}

#[test]
fn object_storage_preserves_process_handles_across_suspension() {
    for suspension in ["await sleep(1);", "await tools.ping({});"] {
        law(OBJECT, "handles.child", suspension, false);
    }
}

#[test]
fn durable_replay_preserves_process_handles_in_containers() {
    law(LIST, "handles[0]", "await sleep(1);", true);
    law(OBJECT, "handles.child", "await sleep(1);", true);
}

#[test]
fn closed_null_container_elements_still_refuse_process_controls() {
    for source in [
        "const handles = [null]; await processes.await({handle:handles[0]});",
        "const handles = {child:null}; await processes.await({handle:handles.child});",
        "const handles = [null]; for (const handle of handles) { await processes.await({handle:handle}); }",
    ] {
        let error = lash_typescript::link(source, &environment())
            .expect_err("closed null is not a process handle");
        assert!(
            error.message.contains("expects { handle: Process }"),
            "{error}"
        );
        assert!(error.message.contains("got { handle: null }"), "{error}");
    }
}
