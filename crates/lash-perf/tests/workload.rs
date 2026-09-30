use lash_perf::workload::{
    CallCounts, CallKind, Generator, OperationId, ToolCallPlan, V1_JSON, V1_SCHEMA_JSON, Workload,
    schema,
};
use rand_chacha::rand_core::RngCore;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn value() -> anyhow::Result<Value> {
    Ok(serde_json::from_str(V1_JSON)?)
}
fn parse(value: &Value) -> anyhow::Result<Workload> {
    Workload::parse(&value.to_string())
}

#[test]
fn workload_schema_is_current_and_every_field_has_provenance() {
    let expected: Value = serde_json::from_str(V1_SCHEMA_JSON).unwrap();
    assert_eq!(expected, serde_json::to_value(schema()).unwrap());
    assert!(
        jsonschema::JSONSchema::compile(&expected)
            .unwrap()
            .is_valid(&value().unwrap())
    );
    let workload = Workload::v1().unwrap();
    assert_eq!(workload.spec().sessions, 200);
    assert_eq!(workload.spec().turns_per_session_s * 200.0, 20.0);
    assert_eq!(workload.spec().minimum_completed_turns, 10000);
    assert_eq!(
        workload.spec().inventory.figments_sha,
        "ea5ee6cacd1079c42490c2548b261d002817a3b4"
    );
    assert!(workload.provenance("/sessions").unwrap().starts_with("I:"));
    assert!(
        workload
            .provenance("/topology/worker_cpu")
            .unwrap()
            .starts_with("V:")
    );
    eprintln!(
        "v1 schema: {} fields with provenance, 200 sessions, 20 turns/s",
        workload.spec().provenance.fields.len()
    );
}

#[test]
fn workload_rejects_unknown_missing_null_and_duplicate_fields_at_every_object() {
    let original = value().unwrap();
    let validator =
        jsonschema::JSONSchema::compile(&serde_json::to_value(schema()).unwrap()).unwrap();
    let mut objects = vec![String::new()];
    objects.extend(
        original
            .as_object()
            .unwrap()
            .iter()
            .filter_map(|(key, val)| val.is_object().then_some(format!("/{key}"))),
    );
    objects.retain(|path| path != "/provenance/fields");
    let mut cases = 0;
    for path in objects {
        let mut bad = original.clone();
        bad.pointer_mut(&path)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), json!(true));
        assert!(parse(&bad).is_err(), "unknown field at {path}");
        assert!(
            !validator.is_valid(&bad),
            "schema accepted unknown field at {path}"
        );
        let fields: Vec<String> = original
            .pointer(&path)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for field in fields {
            let mut bad = original.clone();
            bad.pointer_mut(&path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(&field);
            assert!(parse(&bad).is_err(), "missing {path}/{field}");
            assert!(
                !validator.is_valid(&bad),
                "schema accepted missing {path}/{field}"
            );
            let mut bad = original.clone();
            *bad.pointer_mut(&format!("{path}/{field}")).unwrap() = Value::Null;
            assert!(parse(&bad).is_err(), "null {path}/{field}");
            assert!(
                !validator.is_valid(&bad),
                "schema accepted null {path}/{field}"
            );
            cases += 2;
        }
        cases += 1;
    }
    assert!(
        Workload::parse(&V1_JSON.replacen("\"seed\": 3790", "\"seed\": 3790, \"seed\": 2", 1))
            .is_err()
    );
    eprintln!(
        "strict object rejection: {} cases plus duplicate field",
        cases
    );
}

#[test]
fn workload_rejects_versions_probabilities_limits_and_inconsistent_settings() {
    let cases = [
        ("/format_version", json!(2)),
        ("/generator/version", json!(2)),
        ("/generator/algorithm", json!("ChaCha8")),
        ("/arrival", json!("closed_loop")),
        ("/sessions", json!(0)),
        ("/turns_per_session_s", json!(0)),
        ("/provider/max_concurrent", json!(0)),
        ("/tools/turn_share", json!(1.01)),
        ("/processes/turn_share", json!(-0.01)),
        ("/queued/inputs_per_turn", json!(-1)),
        ("/provider/stream_share", json!("0.5")),
        ("/collection/confidence", json!(0)),
        ("/collection/confidence", json!(1)),
        ("/inventory/figments_sha", json!("bad")),
        ("/saturation_rates_per_session_s", json!([0.1, 0.05])),
        ("/saturation_rates_per_session_s", json!([0.0, 0.1])),
        ("/topology/replication", json!(4)),
        ("/faults/rolling_deploy_s", json!(1199)),
        ("/attachments/count", json!([[1000, 1.0]])),
        ("/provider/chunks", json!(100000)),
        (
            "/provenance/fields/~1sessions",
            json!("production telemetry"),
        ),
    ];
    for (path, replacement) in &cases {
        let mut bad = value().unwrap();
        *bad.pointer_mut(path).unwrap() = replacement.clone();
        assert!(parse(&bad).is_err(), "accepted {path}={replacement}");
    }
    for path in [
        "/prompt_bytes",
        "/input_bytes",
        "/tool_argument_bytes",
        "/tool_result_bytes",
        "/provider_output_bytes",
        "/history_prefill_turns",
        "/tools/fanout",
        "/tools/callback_ms",
        "/processes/fanout",
        "/attachments/count",
        "/attachments/aggregate_bytes",
        "/provider/latency_ms",
    ] {
        for replacement in [
            json!([]),
            json!([[1, 0.2]]),
            json!([[1, 0.5], [1, 0.5]]),
            json!([[1, -1], [2, 2]]),
            json!([[1, 1, 0]]),
        ] {
            let mut bad = value().unwrap();
            *bad.pointer_mut(path).unwrap() = replacement.clone();
            assert!(parse(&bad).is_err(), "accepted {path}={replacement}");
        }
    }
    let mut bad = value().unwrap();
    bad["provenance"]["fields"]
        .as_object_mut()
        .unwrap()
        .remove("/sessions");
    assert!(parse(&bad).is_err());
    let mut bad = value().unwrap();
    bad["provenance"]["fields"]["/typo"] = json!("I");
    assert!(parse(&bad).is_err());
    eprintln!(
        "invalid values: {} scalar cases, 60 distribution cases, 2 provenance cases",
        cases.len()
    );
}

#[test]
fn synthetic_text_and_nested_json_hit_every_size_fixture() {
    let fixture: Value =
        serde_json::from_str(include_str!("../workloads/fixtures/sizes-v1.json")).unwrap();
    let workload = Workload::v1().unwrap();
    let generator = Generator::new(&workload, "sizes").unwrap();
    let arguments = jsonschema::JSONSchema::compile(&lash_perf::workload::tool_schema()).unwrap();
    let results =
        jsonschema::JSONSchema::compile(&lash_perf::workload::tool_result_schema()).unwrap();
    for size in fixture["text_bytes"].as_array().unwrap() {
        let bytes = size.as_u64().unwrap() as u32;
        let text = generator.text(1, 2, "fixture", bytes);
        assert_eq!(text.len(), bytes as usize);
        assert!(text.contains('λ') || text.contains('ø'));
    }
    for size in fixture["json_bytes"].as_array().unwrap() {
        let bytes = size.as_u64().unwrap() as u32;
        let record = generator.record(1, 2, "fixture", bytes).unwrap();
        let call = ToolCallPlan {
            idempotency_key: generator.operation(1, 2).child_key("tool/0", 3),
            argument_bytes: bytes,
            result_bytes: 1024,
            callback_ms: 10,
        };
        let argument = generator.tool_argument(1, 2, &call).unwrap();
        assert!(arguments.is_valid(&argument));
        assert_eq!(argument["record"]["key"], json!("sizes/1/2/tool/0/3"));
        assert_eq!(serde_json::to_vec(&argument).unwrap().len(), bytes as usize);
        assert_eq!(serde_json::to_vec(&record).unwrap().len(), bytes as usize);
        assert!(results.is_valid(&record));
        let result = generator.tool_result(&call.idempotency_key, bytes).unwrap();
        assert!(results.is_valid(&result));
        assert_eq!(serde_json::to_vec(&result).unwrap().len(), bytes as usize);
    }
    assert!(generator.record(0, 0, "too-small", 1).is_err());
    let cramped = ToolCallPlan {
        idempotency_key: generator.operation(1, 2).child_key("tool/0", 0),
        argument_bytes: 64,
        result_bytes: 1024,
        callback_ms: 10,
    };
    assert!(generator.tool_argument(1, 2, &cramped).is_err());
    assert!(generator.tool_result("other/1/2/tool/0/0", 1024).is_err());
    assert!(generator.tool_result("sizes/1/2/child/0", 1024).is_err());
    assert_eq!(
        OperationId::parse("sizes/1/2/tool/0/3").unwrap(),
        (generator.operation(1, 2), "tool/0/3")
    );
    assert!(OperationId::parse("sizes/one/2").is_err());
    eprintln!(
        "size fixtures: 3 UTF-8 text buckets, {} nested JSON buckets",
        fixture["json_bytes"].as_array().unwrap().len()
    );
}

#[test]
fn png_sizes_mime_pixels_crc_and_digest_are_verified() {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let workload = Workload::v1().unwrap();
    let generator = Generator::new(&workload, "png").unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("../workloads/fixtures/sizes-v1.json")).unwrap();
    let mut count = 0;
    for aggregate in fixture["attachment_aggregates"].as_array().unwrap() {
        for files in fixture["attachment_counts"].as_array().unwrap() {
            let aggregate = aggregate.as_u64().unwrap() as u32;
            let files = files.as_u64().unwrap() as u32;
            let mut total = 0;
            for index in 0..files {
                let size = aggregate / files + u32::from(index < aggregate % files);
                let bytes = generator.png(0, u64::from(index), "fixture", size).unwrap();
                let attachment = lash_perf::workload::SyntheticAttachment {
                    blob_key: "fixture".into(),
                    owner_actors: vec![0],
                    media_type: "image/png".into(),
                    sha256: format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(&bytes)),
                    bytes,
                };
                attachment.verify(size).unwrap();
                let mut input = &attachment.bytes[8..];
                while !input.is_empty() {
                    let length = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
                    if &input[4..8] == b"IDAT" {
                        let mut raw = Vec::new();
                        ZlibDecoder::new(&input[8..8 + length])
                            .read_to_end(&mut raw)
                            .unwrap();
                        assert_eq!(raw.len(), 5);
                        assert_eq!(raw[0], 0);
                    }
                    input = &input[12 + length..];
                }
                let mut corrupt = attachment.clone();
                corrupt.bytes[29] ^= 1;
                corrupt.sha256 = format!(
                    "{:x}",
                    <sha2::Sha256 as sha2::Digest>::digest(&corrupt.bytes)
                );
                assert!(corrupt.verify(size).is_err());
                let mut wrong_mime = attachment.clone();
                wrong_mime.media_type = "text/plain".into();
                assert!(wrong_mime.verify(size).is_err());
                let mut wrong_digest = attachment.clone();
                wrong_digest.sha256 = "00".into();
                assert!(wrong_digest.verify(size).is_err());
                assert!(attachment.verify(size + 1).is_err());
                total += size;
                count += 1;
            }
            assert_eq!(total, aggregate);
        }
    }
    eprintln!(
        "PNG fixtures: {count} files across 6 aggregate/count combinations, all corruption checks rejected"
    );
}

#[test]
fn streams_and_retry_ids_are_independent_and_shared_blobs_have_two_owners() {
    let workload = Workload::v1().unwrap();
    let generator = Generator::new(&workload, "identity").unwrap();
    let bytes = |actor, ordinal, purpose| {
        let mut data = [0; 32];
        generator
            .stream(actor, ordinal, purpose)
            .fill_bytes(&mut data);
        data
    };
    let baseline = bytes(0, 0, "prompt");
    assert_eq!(baseline, bytes(0, 0, "prompt"));
    let unique = BTreeSet::from([
        baseline,
        bytes(1, 0, "prompt"),
        bytes(0, 1, "prompt"),
        bytes(0, 0, "input"),
    ]);
    assert_eq!(unique.len(), 4);
    generator.stream(0, 0, "input").next_u64();
    assert_eq!(baseline, bytes(0, 0, "prompt"));
    let mut changed = value().unwrap();
    changed["seed"] = json!(3791);
    let changed_workload = parse(&changed).unwrap();
    let mut changed_bytes = [0; 32];
    Generator::new(&changed_workload, "identity")
        .unwrap()
        .stream(0, 0, "prompt")
        .fill_bytes(&mut changed_bytes);
    assert_ne!(baseline, changed_bytes);
    for ordinal in (0..10000).step_by(10) {
        let left = generator.plan(0, ordinal).unwrap();
        if left.attachments.is_empty() {
            continue;
        }
        let right = generator.plan(1, ordinal).unwrap();
        let left_payload = generator.materialize(&left).unwrap();
        let right_payload = generator.materialize(&right).unwrap();
        for (a, b) in left_payload
            .attachments
            .iter()
            .zip(&right_payload.attachments)
        {
            assert_eq!(a.blob_key, b.blob_key);
            assert_eq!(a.bytes, b.bytes);
            assert_eq!(a.sha256, b.sha256);
            assert_eq!(a.owner_actors, vec![0, 1]);
        }
        assert_eq!(
            left.operation.key(),
            generator.plan(0, ordinal).unwrap().operation.key()
        );
        return;
    }
    panic!("no shared attachment fixture");
}

#[test]
fn provider_cells_and_durable_bodies_parse_and_stream_to_the_sampled_latency() {
    let workload = Workload::v1().unwrap();
    let generator = Generator::new(&workload, "provider").unwrap();
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["tools"],
            "Tools",
            "synthetic",
            "synthetic",
            &lashlang::OperationContract::new(
                lash_perf::workload::tool_schema(),
                lash_perf::workload::tool_result_schema(),
            ),
        )
        .unwrap();
    for (name, arguments) in [
        ("mark", lash_perf::workload::mark_schema()),
        ("attach", lash_perf::workload::attach_schema()),
    ] {
        catalog
            .add_module_operation_contract(
                ["tools"],
                "Tools",
                name,
                name,
                &lashlang::OperationContract::new(arguments, json!({"type": "object"})),
            )
            .unwrap();
    }
    catalog
        .add_module_operation(
            ["processes"],
            "Processes",
            "start",
            "start",
            lashlang::TypeExpr::Object(vec![
                lashlang::TypeField {
                    name: "definition".into(),
                    ty: lashlang::TypeExpr::Process(lashlang::ProcessType::unknown()),
                    optional: false,
                },
                lashlang::TypeField {
                    name: "args".into(),
                    ty: lashlang::TypeExpr::Any,
                    optional: true,
                },
            ]),
            lashlang::TypeExpr::Any,
        )
        .unwrap();
    for operation in ["await", "signal"] {
        catalog
            .add_module_operation(
                ["processes"],
                "Processes",
                operation,
                operation,
                lashlang::TypeExpr::Any,
                lashlang::TypeExpr::Any,
            )
            .unwrap();
    }
    let host = lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::all());
    let mut witnessed = BTreeSet::new();
    for ordinal in 0..300 {
        let mut plan = generator.plan(0, ordinal).unwrap();
        plan.retryable_first_attempt = true;
        let first = generator.provider_response(&plan, 1).unwrap();
        let retry = generator.provider_response(&plan, 2).unwrap();
        assert!(first.retryable);
        assert!(!retry.retryable);
        assert_eq!(first.operation_id, retry.operation_id);
        assert_eq!(first.text, retry.text);
        let cell = format!(
            "<typescript>\n{}\n</typescript>",
            first.cell_source.as_deref().unwrap()
        );
        if cell.len() < plan.provider_output_bytes as usize {
            assert_eq!(first.text.len(), plan.provider_output_bytes as usize);
            witnessed.insert("padded");
        } else {
            assert_eq!(first.text, cell, "an overflowing cell is served whole");
            witnessed.insert("overflow");
        }
        assert_eq!(
            first
                .chunks
                .iter()
                .map(|c| c.text.as_str())
                .collect::<String>(),
            first.text
        );
        assert_eq!(
            first.chunks.last().unwrap().due_ms,
            plan.provider_latency_ms
        );
        assert!(first.chunks.windows(2).all(|c| c[0].due_ms <= c[1].due_ms));
        if let Some(code) = first.cell_source {
            let linked =
                lash_typescript::link(&code, &host).unwrap_or_else(|e| panic!("{e:?}\n{code}"));
            if !plan.child_processes.is_empty() {
                // A process handle read back from a list is null at run
                // time (FIG-4168 smoke): cells keep each handle in a name.
                assert!(!code.contains("handles"), "{code}");
                for index in 0..plan.child_processes.len() {
                    assert!(code.contains(&format!("const h{index}=await processes.start(")));
                }
                let process = linked
                    .artifact
                    .ir()
                    .declarations
                    .iter()
                    .find_map(|declaration| {
                        if let lashlang::Declaration::Process(process) = declaration {
                            Some(process)
                        } else {
                            None
                        }
                    })
                    .expect("child body is a durable module definition");
                assert!(
                    process
                        .signals
                        .iter()
                        .any(|signal| signal.name.as_str() == "resume")
                );
            }
            witnessed.insert("cell");
            if !plan.tool_batches.is_empty() {
                witnessed.insert("tools");
            }
            if !plan.child_processes.is_empty() {
                witnessed.insert("process");
            }
            if plan.child_processes.iter().any(|p| p.parked) {
                witnessed.insert("delayed-signal");
            }
        } else {
            panic!("every primary turn needs an RLM cell");
        }
        if plan.auxiliary_llm_requests > 0 {
            let request = generator.llm_request(&plan, 0).unwrap();
            let plain = generator.llm_response(&request, 1).unwrap();
            assert!(plain.cell_source.is_none());
            assert_eq!(plain.text.len(), request.output_bytes as usize);
            assert_eq!(plain.chunks.last().unwrap().due_ms, request.latency_ms);
            witnessed.insert("plain");
        }
        for process in &plan.host_processes {
            let linked = lash_typescript::link(&generator.process_body(process), &host).unwrap();
            let bodies: Vec<_> = linked
                .artifact
                .ir()
                .declarations
                .iter()
                .filter_map(|declaration| match declaration {
                    lashlang::Declaration::Process(process) => Some(process),
                    _ => None,
                })
                .collect();
            let [body] = bodies.as_slice() else {
                panic!("a host start links exactly one durable body");
            };
            assert_eq!(
                body.signals
                    .iter()
                    .any(|signal| signal.name.as_str() == "resume"),
                process.waits_for_signal()
            );
            witnessed.insert(if process.waits_for_signal() {
                "host-waiting"
            } else {
                "host"
            });
        }
        for queued in &plan.queued_inputs {
            let response = generator.queued_response(&plan, queued).unwrap();
            lash_typescript::link(response.cell_source.as_ref().unwrap(), &host).unwrap();
            assert!(response.text.contains(&queued.idempotency_key));
            witnessed.insert("queued");
        }
    }
    let mut worst = generator.plan(0, 0).unwrap();
    worst.provider_output_bytes = 1024;
    let mut tool = generator.plan(0, 1).unwrap();
    while tool.tool_batches.is_empty() {
        tool = generator.plan(0, tool.operation.ordinal + 1).unwrap();
    }
    let mut call = tool.tool_batches[0][0].clone();
    call.argument_bytes = 32768;
    worst.tool_batches = vec![vec![call; 16]];
    let mut process = generator.plan(0, 1).unwrap();
    while process.child_processes.is_empty() {
        process = generator.plan(0, process.operation.ordinal + 1).unwrap();
    }
    let mut child = process.child_processes[0].clone();
    child.parked = true;
    child.await_result = true;
    worst.child_processes = vec![child; 8];
    let mut attachment = generator.plan(0, 0).unwrap();
    while attachment.attachments.len() < 3 {
        attachment = generator.plan(0, attachment.operation.ordinal + 1).unwrap();
    }
    worst.attachments = attachment.attachments;
    for parallel in [true, false] {
        worst.parallel_tools = parallel;
        let response = generator.provider_response(&worst, 1).unwrap();
        let source = response.cell_source.as_ref().unwrap();
        lash_typescript::link(source, &host).unwrap();
        // The worst cell overflows the smallest bucket: it is served whole.
        assert!(response.text.len() > 1024);
        assert_eq!(
            response.text,
            format!("<typescript>\n{source}\n</typescript>")
        );
    }
    let cron = generator.cron_setup_response().unwrap();
    assert_eq!(cron.operation_id, "provider/cron");
    assert!(cron.text.contains("provider/cron/0"));
    assert!(cron.text.contains(&format!(
        "provider/cron/{}",
        workload.spec().cron.subscriptions - 1
    )));
    assert_eq!(generator.cron_tick_key(3, 7), "provider/cron/3/tick/7");
    assert_eq!(
        witnessed,
        BTreeSet::from([
            "cell",
            "tools",
            "process",
            "delayed-signal",
            "plain",
            "host",
            "host-waiting",
            "queued",
            "padded",
            "overflow"
        ])
    );
    let mut counts = CallCounts::default();
    for kind in [
        CallKind::Fresh,
        CallKind::ReplayedEffect,
        CallKind::Retry,
        CallKind::Retry,
    ] {
        counts.record(kind);
    }
    assert_eq!(
        (counts.fresh, counts.replayed_effects, counts.retries),
        (1, 1, 2)
    );
    eprintln!(
        "provider fixtures: 300 responses and identical retries; plain, tools, durable body, await and delayed signal parse"
    );
}

#[test]
fn mix_fixture_covers_all_distributions_overlaps_and_auxiliary_rates() {
    let fixture: Value =
        serde_json::from_str(include_str!("../workloads/fixtures/mix-v1.json")).unwrap();
    let workload = Workload::v1().unwrap();
    let generator = Generator::new(&workload, "mix").unwrap();
    let samples = fixture["samples"].as_u64().unwrap();
    let mut distributions: BTreeMap<&str, BTreeMap<u32, u64>> = BTreeMap::new();
    let mut shares: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    let mut gap_total = 0.0;
    for index in 0..samples {
        let actor = index % 200;
        let ordinal = index / 200;
        let plan = generator.plan(actor, ordinal).unwrap();
        let mut bucket = |name, value| {
            *distributions
                .entry(name)
                .or_default()
                .entry(value)
                .or_default() += 1;
        };
        bucket(
            "history_prefill_turns",
            generator.prefill_turns(actor, ordinal),
        );
        bucket("prompt_bytes", plan.prompt_bytes);
        bucket("input_bytes", plan.input_bytes);
        bucket("provider_output_bytes", plan.provider_output_bytes);
        bucket("latency_ms", plan.provider_latency_ms);
        for batch in &plan.tool_batches {
            bucket("tool_fanout", batch.len() as u32);
            for call in batch {
                bucket("tool_argument_bytes", call.argument_bytes);
                bucket("tool_result_bytes", call.result_bytes);
                bucket("callback_ms", call.callback_ms);
            }
        }
        if !plan.child_processes.is_empty() {
            bucket("process_fanout", plan.child_processes.len() as u32);
        }
        if !plan.attachments.is_empty() {
            bucket("attachment_count", plan.attachments.len() as u32);
            bucket(
                "attachment_aggregate",
                plan.attachments.iter().map(|a| a.bytes).sum(),
            );
        }
        let mut share = |name, count, population| {
            let entry = shares.entry(name).or_default();
            entry.0 += count;
            entry.1 += population;
        };
        share("tools", u64::from(!plan.tool_batches.is_empty()), 1);
        share("processes", u64::from(!plan.child_processes.is_empty()), 1);
        share(
            "overlap",
            u64::from(!plan.tool_batches.is_empty() && !plan.child_processes.is_empty()),
            1,
        );
        share("attachments", u64::from(!plan.attachments.is_empty()), 1);
        if !plan.tool_batches.is_empty() {
            share("parallel", u64::from(plan.parallel_tools), 1);
        }
        for p in &plan.child_processes {
            share("park", u64::from(p.parked), 1);
            share("await", u64::from(p.await_result), 1);
        }
        for p in &plan.host_processes {
            share("signal", u64::from(p.signal), 1);
            share("process_cancel", u64::from(p.cancel), 1);
        }
        share("queued", plan.queued_inputs.len() as u64, 1);
        for queued in &plan.queued_inputs {
            share("queued_active", u64::from(queued.during_active_turn), 1);
            share("queued_cancel", u64::from(queued.cancel), 1);
        }
        for (name, count) in [
            ("llm", plan.auxiliary_llm_requests),
            ("host", plan.host_processes.len() as u32),
            ("occurrences", plan.external_occurrences),
            ("trigger_edits", plan.trigger_edits),
            ("promotion_reads", plan.promotion_reads),
        ] {
            share(name, u64::from(count), 1);
        }
        for (name, yes) in [
            ("cancel", plan.cancel),
            ("delete", plan.delete),
            ("reconnect", plan.reconnect),
            ("stream", plan.provider_streamed),
            ("retry", plan.retryable_first_attempt),
        ] {
            share(name, u64::from(yes), 1);
        }
        gap_total += plan.arrival_gap_s;
    }
    let assert_share = |name: &str, hits: u64, n: u64, expected: f64| {
        let actual = hits as f64 / n as f64;
        let tolerance = (fixture["sigma"].as_f64().unwrap()
            * (expected * (1.0 - expected) / n as f64).sqrt())
        .max(fixture["minimum_tolerance"].as_f64().unwrap());
        assert!(
            (actual - expected).abs() <= tolerance,
            "{name}: expected {expected}, got {actual}, tolerance {tolerance}, n={n}"
        );
    };
    for (name, expected) in fixture["shares"].as_object().unwrap() {
        let &(hits, n) = shares.get(name.as_str()).unwrap();
        assert_share(name, hits, n, expected.as_f64().unwrap());
    }
    for (name, expected) in fixture["distributions"].as_object().unwrap() {
        let actual = distributions.get(name.as_str()).unwrap();
        assert_eq!(actual.len(), expected.as_array().unwrap().len(), "{name}");
        let n = actual.values().sum();
        for pair in expected.as_array().unwrap() {
            let val = pair[0].as_u64().unwrap() as u32;
            assert_share(name, actual[&val], n, pair[1].as_f64().unwrap());
        }
    }
    assert!((gap_total / samples as f64 - 10.0).abs() < 0.5);
    eprintln!(
        "mix fixtures: {samples} plans; {} distributions, {} shares; mean arrival gap {:.4}s",
        distributions.len(),
        shares.len(),
        gap_total / samples as f64
    );
}

#[test]
fn primary_turn_cells_have_standalone_delimiters_even_without_tools_or_processes() {
    let workload = Workload::v1().unwrap();
    let generator = Generator::new(&workload, "framing").unwrap();
    let mut plan = (0..100)
        .map(|ordinal| generator.plan(0, ordinal).unwrap())
        .find(|plan| !plan.tool_batches.is_empty())
        .expect("tool fixture");
    let response = generator.provider_response(&plan, 1).unwrap();
    assert!(
        response
            .text
            .lines()
            .any(|line| line.trim() == "<typescript>"),
        "RLM only executes a cell when its opening delimiter occupies its own line"
    );
    assert_eq!(response.text.lines().last(), Some("</typescript>"));
    plan.tool_batches.clear();
    plan.child_processes.clear();
    let response = generator.provider_response(&plan, 1).unwrap();
    assert!(
        response.cell_source.is_some(),
        "every primary turn uses RLM"
    );
    assert!(
        response
            .text
            .lines()
            .any(|line| line.trim() == "<typescript>")
    );
    assert_eq!(response.text.len(), plan.provider_output_bytes as usize);
}

#[test]
fn auxiliary_llm_requests_keep_independent_retry_ids_and_byte_sizes() {
    let mut settings = value().unwrap();
    settings["llm_requests_per_turn"] = json!(2.5);
    settings["provenance"]["fields"]["/llm_requests_per_turn"] =
        json!("I: auxiliary identity fixture");
    let workload = parse(&settings).unwrap();
    let generator = Generator::new(&workload, "auxiliary").unwrap();
    let turn = generator.plan(0, 0).unwrap();
    assert!(turn.auxiliary_llm_requests >= 2);
    let mut keys = BTreeSet::from([turn.operation.key()]);
    for index in 0..turn.auxiliary_llm_requests {
        let mut request = generator.llm_request(&turn, index).unwrap();
        request.retryable_first_attempt = true;
        let first = generator.llm_response(&request, 1).unwrap();
        let retry = generator.llm_response(&request, 2).unwrap();
        assert!(keys.insert(first.operation_id.clone()));
        assert_eq!(first.operation_id, retry.operation_id);
        assert_eq!(first.text, retry.text);
        assert!(first.retryable && !retry.retryable);
        assert!(first.cell_source.is_none());
        assert_eq!(
            generator.llm_prompt(&request).len(),
            request.prompt_bytes as usize
        );
        assert_eq!(first.text.len(), request.output_bytes as usize);
        assert_eq!(
            first
                .chunks
                .iter()
                .map(|chunk| chunk.text.as_str())
                .collect::<String>(),
            first.text
        );
        assert_eq!(first.chunks.last().unwrap().due_ms, request.latency_ms);
    }
    assert!(
        generator
            .llm_request(&turn, turn.auxiliary_llm_requests)
            .is_err()
    );
    eprintln!(
        "auxiliary fixtures: {} independently keyed requests, exact prompt/output sizes and unchanged retries",
        turn.auxiliary_llm_requests
    );
}

#[test]
fn smoke_workload_covers_every_durable_operation_class_in_its_first_turns() {
    let workload = Workload::smoke_v1().unwrap();
    assert_eq!(
        Workload::named("smoke-v1").unwrap().sha256(),
        workload.sha256()
    );
    assert_ne!(Workload::v1().unwrap().sha256(), workload.sha256());
    assert!(Workload::named("figments-v2").is_err());
    let generator = Generator::new(&workload, "smoke").unwrap();
    let mut covered: BTreeMap<&str, u64> = BTreeMap::new();
    let mut hit = |class: &'static str, yes: bool| {
        *covered.entry(class).or_default() += u64::from(yes);
    };
    let sessions = u64::from(workload.spec().sessions);
    for actor in 0..sessions {
        for ordinal in 0..lash_perf::workload::SMOKE_TURNS_PER_SESSION {
            let plan = generator.plan(actor, ordinal).unwrap();
            hit("turn", true);
            hit(
                "tools-parallel",
                !plan.tool_batches.is_empty() && plan.parallel_tools,
            );
            hit(
                "tools-serial",
                !plan.tool_batches.is_empty() && !plan.parallel_tools,
            );
            hit("child-process", !plan.child_processes.is_empty());
            hit(
                "child-parked",
                plan.child_processes.iter().any(|process| process.parked),
            );
            for process in &plan.host_processes {
                hit("host-process", true);
                hit(
                    "host-signalled",
                    process.waits_for_signal() && !process.cancel,
                );
                hit("host-cancelled", process.cancel);
            }
            hit("attachment", !plan.attachments.is_empty());
            let shared = plan.attachments.iter().any(|attachment| {
                attachment.owner_actors.len() == 2
                    && attachment
                        .owner_actors
                        .iter()
                        .all(|owner| *owner < sessions)
            });
            hit("attachment-shared", shared);
            for queued in &plan.queued_inputs {
                hit("queued-active", queued.during_active_turn);
                hit("queued-after", !queued.during_active_turn);
                hit("queued-cancelled", queued.cancel);
            }
            hit("turn-cancel", plan.cancel);
            hit("delete", plan.delete);
            hit("rotate", plan.rotate);
            hit("provider-retry", plan.retryable_first_attempt);
        }
    }
    eprintln!("smoke coverage over {sessions} sessions: {covered:?}");
    for (class, count) in &covered {
        assert!(*count > 0, "the smoke workload never exercises {class}");
    }
    assert_eq!(covered.len(), 17);
}
