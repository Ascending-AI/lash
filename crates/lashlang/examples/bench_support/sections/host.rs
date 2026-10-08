impl ExecutionHost for BenchHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                let empty = Record::new();
                let args = operation
                    .args
                    .first()
                    .and_then(Value::as_record)
                    .unwrap_or(&empty);
                bench_resource_call(&operation, args).map(AbilityOutcome::Value)
            }
            AbilityOp::ResourceOperationBatch(batch) => {
                let results = batch
                    .leaves
                    .iter()
                    .map(|leaf| match leaf {
                        lashlang::ResourceOperationBatchLeaf::Operation(operation) => {
                            let empty = Record::new();
                            let args = operation
                                .args
                                .first()
                                .and_then(Value::as_record)
                                .unwrap_or(&empty);
                            lashlang::ResourceOperationOutcome::from_result(bench_resource_call(
                                operation, args,
                            ))
                        }
                        lashlang::ResourceOperationBatchLeaf::Timer(_) => {
                            lashlang::ResourceOperationOutcome::Value(Value::Undefined)
                        }
                    })
                    .collect();
                Ok(AbilityOutcome::ResourceOperationBatch(
                    batch.answer_in_leaf_order(results),
                ))
            }
            AbilityOp::Await(handle) => {
                let record = handle
                    .as_record()
                    .ok_or_else(|| ExecutionHostError::new("expected handle record"))?;
                Ok(AbilityOutcome::Value(
                    record.get("value").cloned().unwrap_or(Value::Null),
                ))
            }
            AbilityOp::Print(_) => Ok(AbilityOutcome::Unit),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

fn bench_resource_call(
    operation: &lashlang::ResourceOperation,
    args: &Record,
) -> Result<Value, ExecutionHostError> {
    let host_operation = match &operation.receiver {
        Value::Resource(receiver) => benchmark_host_environment()
            .resources
            .resolve_module_operation(
                &receiver.resource_type,
                &receiver.alias,
                &operation.operation,
            )
            .map(|binding| binding.host_operation)
            .ok_or_else(|| {
                ExecutionHostError::new(format!(
                    "module `{}` of type `{}` does not expose operation `{}`",
                    receiver.alias, receiver.resource_type, operation.operation
                ))
            })?,
        _ => operation.operation.as_str(),
    };
    bench_call(host_operation, args)
}

fn bench_call(name: &str, args: &Record) -> Result<Value, ExecutionHostError> {
    match name {
        "start_process_handle" => {
            // The start's own arguments ride in `args`, where the stub name the
            // bench answers with sits beside the process's parameters.
            let Some(start_args) = args.get("args").and_then(Value::as_record) else {
                return Err(ExecutionHostError::new("start expects an `args` record"));
            };
            let Some(Value::String(process)) = start_args.get("tool") else {
                return Err(ExecutionHostError::new("start expects a `tool` name"));
            };
            BenchHost::task_handle(process.as_ref(), start_args)
        }
        "cancel_process_handle" => Ok(args.get("handle").cloned().unwrap_or(Value::Null)),
        "echo" => Ok(args.get("value").cloned().unwrap_or(Value::Null)),
        "boom" => Err(ExecutionHostError::new("explicit failure for benchmark")),
        "run_job" => {
            let mut record = Record::default();
            record.insert("status".to_string(), Value::String("completed".into()));
            record.insert("done".to_string(), Value::Bool(true));
            record.insert("running".to_string(), Value::Bool(false));
            record.insert("exit_code".to_string(), Value::Number(1.0));
            record.insert(
                "output".to_string(),
                Value::String(
                    format!(
                        "ran: {}",
                        args.get("target")
                            .and_then(|value| match value {
                                Value::String(text) => Some(text.as_str()),
                                _ => None,
                            })
                            .unwrap_or("")
                    )
                    .into(),
                ),
            );
            Ok(Value::Record(Arc::new(record)))
        }
        "llm_query" | "query_llm" => {
            let mut record = Record::default();
            record.insert(
                "text".to_string(),
                Value::String("benchmark summary".into()),
            );
            record.insert("tokens".to_string(), Value::Number(42.0));
            Ok(Value::Record(Arc::new(record)))
        }
        "spawn_agent" | "spawn_child" => {
            let task = args
                .get("task")
                .and_then(|value| match value {
                    Value::String(text) => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("agent");
            let mut record = Record::default();
            record.insert(
                "claim".to_string(),
                Value::String(format!("done:{task}").into()),
            );
            Ok(Value::Record(Arc::new(record)))
        }
        "list_process_handles" => Ok(process_handles_record()),
        "continue_as" => Ok(continue_as_record(args)),

        _ => Err(unknown_tool(name)),
    }
}

fn continue_as_record(args: &Record) -> Value {
    let seed = args.get("seed").and_then(Value::as_record);
    let mut seed_keys = Vec::new();
    let mut projected_count = 0usize;
    let mut global_count = 0usize;
    if let Some(seed) = seed {
        for (key, value) in seed.iter() {
            seed_keys.push(Value::String(key.into()));
            if matches!(value, Value::Projected(_)) {
                projected_count += 1;
            } else {
                global_count += 1;
            }
        }
    }

    let mut record = Record::default();
    record.insert("ok".to_string(), Value::Bool(true));
    record.insert(
        "frame_key".to_string(),
        Value::String(
            "frame-key/v2/7516a6c3aefb1a9e2a39e3c9178e8a241ffa1da902f032f93c1ae9149e4dae33".into(),
        ),
    );
    record.insert(
        "task".to_string(),
        args.get("task")
            .cloned()
            .unwrap_or_else(|| Value::String("continue".into())),
    );
    record.insert("seed_keys".to_string(), Value::List(seed_keys.into()));
    record.insert(
        "projected_count".to_string(),
        Value::Number(projected_count as f64),
    );
    record.insert(
        "global_count".to_string(),
        Value::Number(global_count as f64),
    );
    Value::Record(Arc::new(record))
}

fn string_ref(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value.as_str()),
        _ => None,
    }
}

fn process_handles_record() -> Value {
    let mut chunk_1 = Record::default();
    chunk_1.insert(
        lash_sansio::handle::HANDLE_FIELD.to_string(),
        Value::String(lash_sansio::handle::HANDLE_KIND.into()),
    );
    chunk_1.insert(
        "id".to_string(),
        Value::String(
            lash_sansio::handle::HandleId::process(&lash_sansio::ProcessId::fixture("spawn-one"))
                .as_str()
                .into(),
        ),
    );
    chunk_1.insert("process_id".to_string(), Value::String("spawn-one".into()));
    chunk_1.insert("tool".to_string(), Value::String("spawn_child".into()));
    chunk_1.insert("value".to_string(), spawn_child_value("inspect auth"));

    let mut chunk_2 = Record::default();
    chunk_2.insert(
        lash_sansio::handle::HANDLE_FIELD.to_string(),
        Value::String(lash_sansio::handle::HANDLE_KIND.into()),
    );
    chunk_2.insert(
        "id".to_string(),
        Value::String(
            lash_sansio::handle::HandleId::process(&lash_sansio::ProcessId::fixture("spawn-two"))
                .as_str()
                .into(),
        ),
    );
    chunk_2.insert("process_id".to_string(), Value::String("spawn-two".into()));
    chunk_2.insert("tool".to_string(), Value::String("spawn_child".into()));
    chunk_2.insert("value".to_string(), spawn_child_value("inspect api"));

    Value::List(
        vec![
            Value::Record(Arc::new(chunk_1)),
            Value::Record(Arc::new(chunk_2)),
        ]
        .into(),
    )
}

fn spawn_child_value(name: &str) -> Value {
    let mut record = Record::default();
    record.insert(
        "claim".to_string(),
        Value::String(format!("done:{name}").into()),
    );
    Value::Record(Arc::new(record))
}

fn unknown_tool(name: &str) -> ExecutionHostError {
    ExecutionHostError::new(format!("unknown tool: {name}"))
}

impl BenchHost {
    fn task_handle(name: &str, args: &Record) -> Result<Value, ExecutionHostError> {
        match name {
            "echo" | "query_llm" | "spawn_child" | "continue_as" => {
                let mut record = Record::default();
                record.insert(
                    lash_sansio::handle::HANDLE_FIELD.to_string(),
                    Value::String(lash_sansio::handle::HANDLE_KIND.into()),
                );
                record.insert(
                    "id".to_string(),
                    Value::String(
                        lash_sansio::handle::HandleId::process(&lash_sansio::ProcessId::fixture(
                            name,
                        ))
                        .as_str()
                        .into(),
                    ),
                );
                record.insert("tool".to_string(), Value::String(name.to_string().into()));
                record.insert("value".to_string(), bench_call(name, args)?);
                Ok(Value::Record(Arc::new(record)))
            }
            _ => Err(unknown_tool(name)),
        }
    }
}
