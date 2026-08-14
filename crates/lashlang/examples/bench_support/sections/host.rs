impl ExecutionHost for BenchHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                let empty = Record::new();
                let args = operation
                    .args
                    .first()
                    .and_then(Value::as_record)
                    .unwrap_or(&empty);
                bench_resource_call(&operation, args).map(AbilityResult::Value)
            }
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                lashlang::ResourceOperationBatchResult::settled_in_input_order(batch
                        .operations
                        .into_iter()
                        .map(|operation| {
                            let empty = Record::new();
                            let args = operation
                                .args
                                .first()
                                .and_then(Value::as_record)
                                .unwrap_or(&empty);
                            lashlang::ResourceOperationResult::from_result(bench_resource_call(
                                &operation, args,
                            ))
                        })
                        .collect()),
            )),
            AbilityOp::StartProcess(start) => {
                Self::task_handle(&start.process_name, &start.args).map(AbilityResult::Value)
            }
            AbilityOp::Await(handle) => {
                let record = handle
                    .as_record()
                    .ok_or_else(|| ExecutionHostError::new("expected handle record"))?;
                Ok(AbilityResult::Value(
                    record.get("value").cloned().unwrap_or(Value::Null),
                ))
            }
            AbilityOp::Cancel(handle) => Ok(AbilityResult::Value(handle)),
            AbilityOp::Print(_) => Ok(AbilityResult::Unit),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
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
            .map(|binding| binding.host_operation.as_str())
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
        "echo" => Ok(args.get("value").cloned().unwrap_or(Value::Null)),
        "boom" => Err(ExecutionHostError::new("explicit failure for benchmark")),
        "exec_command" => {
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
                        args.get("cmd")
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
        "triggers.register" => Ok(trigger_register_record(args)),
        "triggers.list" => Ok(trigger_list_value(args)),
        "triggers.disable" => Ok(trigger_mutation_record(args, "disabled")),
        _ => Err(unknown_tool(name)),
    }
}

fn trigger_mutation_record(args: &Record, disposition: &str) -> Value {
    let mut record = Record::default();
    record.insert(
        "subscription_key".to_string(),
        args.get("subscription_key")
            .cloned()
            .unwrap_or_else(|| Value::String("bench-trigger".into())),
    );
    record.insert("revision".to_string(), Value::Number(2.0));
    record.insert("disposition".to_string(), Value::String(disposition.into()));
    record.insert("enabled".to_string(), Value::Bool(false));
    Value::Record(Arc::new(record))
}

fn trigger_register_record(args: &Record) -> Value {
    let source_type = args
        .get("source")
        .cloned()
        .and_then(|source| serde_json::to_value(source).ok())
        .and_then(|source| HostDescriptor::decode(&source).ok())
        .map(|source| source.source_type)
        .unwrap_or_else(|| "unknown.Source".to_string());
    let process_name = args
        .get("target")
        .and_then(Value::as_record)
        .and_then(|target| target.get(LASH_PROCESS_NAME_KEY))
        .and_then(string_ref)
        .unwrap_or("target");

    let mut record = Record::default();
    record.insert("type".to_string(), Value::String("trigger_handle".into()));
    record.insert(
        "id".to_string(),
        Value::String(format!("trigger:{source_type}:{process_name}").into()),
    );
    record.insert(
        "source_type".to_string(),
        Value::String(source_type.clone().into()),
    );
    record.insert(
        "process_name".to_string(),
        Value::String(process_name.to_string().into()),
    );
    Value::Record(Arc::new(record))
}

fn trigger_list_value(args: &Record) -> Value {
    let process_name = args
        .get("target")
        .and_then(Value::as_record)
        .and_then(|target| target.get(LASH_PROCESS_NAME_KEY))
        .and_then(string_ref)
        .unwrap_or("target");
    let source_type = match process_name {
        "daily_digest" => "cron.Schedule",
        "on_button" => "ui.button.pressed",
        _ => "unknown.Source",
    };

    let mut target = Record::default();
    target.insert(
        "process_name".to_string(),
        Value::String(process_name.to_string().into()),
    );
    target.insert("inputs".to_string(), trigger_inputs_value("event"));

    let mut route = Record::default();
    route.insert(
        "handle".to_string(),
        Value::String(format!("trigger:{source_type}:{process_name}").into()),
    );
    route.insert(
        "source_type".to_string(),
        Value::String(source_type.to_string().into()),
    );
    route.insert("source".to_string(), trigger_source_value(source_type));
    route.insert("target".to_string(), Value::Record(Arc::new(target)));
    route.insert("enabled".to_string(), Value::Bool(true));
    Value::List(vec![Value::Record(Arc::new(route))].into())
}

fn trigger_inputs_value(input_name: &str) -> Value {
    let mut event = Record::default();
    event.insert("kind".to_string(), Value::String("event".into()));
    let mut inputs = Record::default();
    inputs.insert(input_name.to_string(), Value::Record(Arc::new(event)));
    Value::Record(Arc::new(inputs))
}

fn trigger_source_value(source_type: &str) -> Value {
    let encoded = HostDescriptor::encode(source_type, serde_json::json!({}))
        .expect("benchmark trigger source should encode");
    from_json(encoded)
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
            "frame-key/v1/7516a6c3aefb1a9e2a39e3c9178e8a241ffa1da902f032f93c1ae9149e4dae33".into(),
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
    chunk_1.insert("__handle__".to_string(), Value::String("process".into()));
    chunk_1.insert("id".to_string(), Value::String("spawn-one".into()));
    chunk_1.insert("process_id".to_string(), Value::String("spawn-one".into()));
    chunk_1.insert("tool".to_string(), Value::String("spawn_child".into()));
    chunk_1.insert("value".to_string(), spawn_child_value("inspect auth"));

    let mut chunk_2 = Record::default();
    chunk_2.insert("__handle__".to_string(), Value::String("process".into()));
    chunk_2.insert("id".to_string(), Value::String("spawn-two".into()));
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
                record.insert("__handle__".to_string(), Value::String("process".into()));
                record.insert("tool".to_string(), Value::String(name.to_string().into()));
                record.insert("value".to_string(), bench_call(name, args)?);
                Ok(Value::Record(Arc::new(record)))
            }
            _ => Err(unknown_tool(name)),
        }
    }
}
