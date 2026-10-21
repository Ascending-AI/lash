use super::*;
use lash::plugins::*;
use lash::tools::*;
use serde_json::{Value, json};
use std::sync::Mutex;

#[derive(Default)]
struct Recorded {
    calls: Mutex<Vec<Value>>,
    ids: Mutex<Vec<String>>,
    pause_at: std::sync::atomic::AtomicUsize,
    reached: tokio::sync::Notify,
}
#[lash::async_trait]
impl ToolProvider for Recorded {
    fn tool_manifests(&self, config: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
        runner::Runner.tool_manifests(config)
    }
    fn resolve_contract(&self, name: &str, config: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        runner::Runner.resolve_contract(name, config)
    }
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        self.calls.lock().unwrap().push(call.args.clone());
        let ordinal = {
            let mut ids = self.ids.lock().unwrap();
            ids.push(call.context.call_id().to_string());
            ids.len()
        };
        if self
            .pause_at
            .compare_exchange(
                ordinal,
                0,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            self.reached.notify_one();
            // Simulate a host dying before this Repeatable attempt commits an outcome.
            std::future::pending::<()>().await;
        }
        runner::Runner.execute(call).await
    }
}

fn response(code: &str, id: &str) -> lash::provider::LlmResponse {
    lash::provider::LlmResponse {
        parts: vec![lash::direct::LlmOutputPart::ToolCall {
            call_id: id.into(),
            tool_name: "execute_code".into(),
            input_json: json!({"code":code}).to_string(),
            replay: None,
        }],
        ..Default::default()
    }
}

fn data_only(value: &Value) {
    match value {
        Value::Object(fields) => {
            assert!(!fields.contains_key(lash::workflow::document::FUNCTION_VALUE_TAG));
            assert!(!fields.contains_key("callback"));
            for value in fields.values() {
                data_only(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                data_only(value);
            }
        }
        _ => {}
    }
}

/// DF-SPLIT: two inputs, both dialects, exact counts, maximal data-only runs,
/// zero performs for viaJs, and saved callbacks reopened from SQLite state.
#[tokio::test]
async fn df_split() {
    evaluator_contract();
    let packages = packages().expect("the host-owned package builds");
    separated_runs(packages.clone()).await;
    for python in [false, true] {
        for (text, expected) in [
            (
                "Red blue red green blue red",
                json!([["red", "3"], ["blue", "2"], ["green", "1"]]),
            ),
            (
                "Moon sun moon star sun moon",
                json!([["moon", "3"], ["sun", "2"], ["star", "1"]]),
            ),
        ] {
            for expressions in [false, true] {
                let recorded = Arc::new(Recorded::default());
                let plugin = Arc::new(StaticPluginFactory::new(
                    PluginDeclaration::initial("dataframe-host"),
                    PluginSpec::new()
                        .with_tool_provider(lash::tool_provider_key!("runner"), recorded.clone()),
                ));
                let mut harness = lash::testing::PluginHarness::new(
                    plugin,
                    lash::testing::HostPolicy::standard(),
                )
                .await
                .unwrap();
                let dialect = if python {
                    lash::rlm::CellDialect::python()
                } else {
                    lash::rlm::CellDialect::typescript()
                };
                let workers = lash::vm::WorkerService::default().with_packages(packages.clone());
                let protocol = lash::rlm::RlmProtocolPluginFactory::new(
                    lash::rlm::RlmProtocolPluginConfig::builder()
                        .channel(lash::rlm::RlmChannel::NativeTool)
                        .instruction_limit(lash::rlm::InstructionBound::instructions(10_000_000))
                        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                        .build(),
                    dialect,
                )
                .with_worker_service(workers)
                .with_packages(packages.clone());
                harness.install_protocol(Arc::new(protocol)).await.unwrap();
                harness.create(PluginOptions::default()).await.unwrap();
                let input = serde_json::to_string(text).unwrap();
                // Park after construction, retaining the actual callback-bearing value;
                // collect in the next cell after destroying all runtime tasks.
                let built = match (python, expressions) {
                    (false, false) => format!(
                        r#"const q = dataframe.DataQuanta.readText({input})
                        .flatMap((line: string) => line.toLowerCase().split(/\W+/))
                        .filterBy((w: string) => w !== "").map((w: string) => [w, 1])
                        .reduceByKeyFn((p) => p[0], (a,b) => [a[0], a[1] + b[1]])
                        .map((p) => [p[0], String(p[1])]);"#
                    ),
                    (false, true) => format!(
                        r#"const {{col,lit,count}} = dataframe;
                        const q = dataframe.DataQuanta.readText({input})
                        .flatMap((line: string) => line.toLowerCase().split(/\W+/))
                        .filter(col("value").neq(lit("")))
                        .reduceByKey([col("value")], [count().alias("n")])
                        .sort(col("n").sort({{ascending:false}}), col("value")).limit(5);"#
                    ),
                    (true, false) => format!(
                        r#"q = (dataframe.DataQuanta.read_text({input})
    .flat_map(lambda line: line.lower().split()).filter_by(lambda w: w != "")
    .map(lambda w: [w, 1]).reduce_by_key_fn(lambda p: p[0], lambda a,b: [a[0], a[1]+b[1]])
    .map(lambda p: [p[0], str(p[1])]))"#
                    ),
                    (true, true) => format!(
                        r#"col,lit,count = dataframe.col,dataframe.lit,dataframe.count
q = (dataframe.DataQuanta.read_text({input}).flat_map(lambda line: line.lower().split())
    .filter(col("value").neq(lit(""))).reduce_by_key([col("value")],[count().alias("n")])
    .sort(col("n").sort(ascending=False),col("value")).limit(5))"#
                    ),
                };
                let baseline = if python {
                    format!(
                        "{}\nbaseline = await {}({input})\n",
                        PYTHON,
                        if expressions { "via_exprs" } else { "via_js" }
                    )
                } else {
                    format!(
                        "{}\nconst baseline = await {}({input});\n",
                        TYPESCRIPT.replace("export ", ""),
                        if expressions { "viaExprs" } else { "viaJs" }
                    )
                };
                let built = baseline + &built;
                let collected = if python {
                    "rows = await q.collect_rows()\nawait control.finish([baseline, rows])"
                } else {
                    "const rows = await q.collectRows(); await control.finish([baseline, rows]);"
                };
                harness.script([response(&built, "build"), response(collected, "collect")]);
                harness.pause_model_after(1);
                let handle = harness
                    .send(lash::TurnInput::text("Count the words"))
                    .await
                    .unwrap();
                let input_id = handle.input_id().clone();
                drop(handle);
                harness.wait_for_model_pause().await.unwrap();
                assert_eq!(
                    recorded.calls.lock().unwrap().len(),
                    usize::from(expressions)
                );
                harness.crash();
                harness.reopen().await.unwrap();
                let output = harness
                    .session()
                    .await
                    .unwrap()
                    .attach(input_id)
                    .output()
                    .await
                    .unwrap();
                let result = output.finished().unwrap_or_else(|| panic!("{output:?}")).1;
                assert_eq!(
                    result[0], result[1],
                    "parked pipeline equals uninterrupted sample"
                );
                let actual = &result[1];
                if expressions {
                    assert_eq!(actual, &expected);
                } else {
                    let mut actual = actual.as_array().unwrap().clone();
                    let mut expected = expected.as_array().unwrap().clone();
                    actual.sort_by_key(Value::to_string);
                    expected.sort_by_key(Value::to_string);
                    assert_eq!(actual, expected);
                }
                let calls = recorded.calls.lock().unwrap();
                assert_eq!(calls.len(), 2 * usize::from(expressions));
                for call in calls.iter() {
                    data_only(call);
                    assert_eq!(call["plan"].as_array().unwrap().len(), 4);
                }
            }
        }
    }
}

/// DF-SPLIT also pins placement when a callback separates two native runs.
async fn separated_runs(packages: Arc<lash::package::HeldPackages>) {
    let recorded = Arc::new(Recorded::default());
    let plugin = Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("dataframe-host"),
        PluginSpec::new().with_tool_provider(lash::tool_provider_key!("runner"), recorded.clone()),
    ));
    let mut harness =
        lash::testing::PluginHarness::new(plugin, lash::testing::HostPolicy::standard())
            .await
            .unwrap();
    publication(harness.core().unwrap().backend().clone()).await;
    let protocol = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::NativeTool)
            .instruction_limit(lash::rlm::InstructionBound::instructions(10_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        lash::rlm::CellDialect::typescript(),
    )
    .with_packages(packages.clone());
    harness.install_protocol(Arc::new(protocol)).await.unwrap();
    harness.create(PluginOptions::default()).await.unwrap();
    let code = r#"const {col,lit,count} = dataframe;
        const q = dataframe.DataQuanta.readText("Red blue red")
        .flatMap((s: string) => s.toLowerCase().split(/\W+/))
        .filter(col("value").neq(lit("")))
        .reduceByKey([col("value")], [count().alias("n")])
        .map((row) => row)
        .sort(col("n").sort({ascending:false}), col("value")).limit(5);
        const explanation = await q.explain();
        const rows = await q.collectRows(); await control.finish([rows, explanation]);"#;
    recorded
        .pause_at
        .store(4, std::sync::atomic::Ordering::SeqCst);
    harness.script([response(code, "cut")]);
    let handle = harness
        .send(lash::TurnInput::text("Run two native stretches"))
        .await
        .unwrap();
    let input = handle.input_id().clone();
    drop(handle);
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        recorded.reached.notified(),
    )
    .await
    .unwrap();
    assert_eq!(recorded.calls.lock().unwrap().len(), 4);
    let turn = harness
        .session()
        .await
        .unwrap()
        .current_turn()
        .await
        .unwrap()
        .unwrap()
        .clone();
    harness.crash();
    harness.reopen().await.unwrap();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        harness.session().await.unwrap().attach(input).output(),
    )
    .await
    .unwrap()
    .unwrap();
    let value = output.finished().unwrap_or_else(|| panic!("{output:?}")).1;
    assert_eq!(value[0], json!([["red", "2"], ["blue", "1"]]));
    assert_eq!(value[1].as_array().unwrap().len(), 4);
    let ledger = harness
        .session()
        .await
        .unwrap()
        .read_turn_tool_calls(&turn)
        .await
        .unwrap();
    assert_eq!(
        ledger
            .iter()
            .filter(|call| call.tool == "dataframe.run")
            .count(),
        2
    );
    assert_eq!(
        ledger
            .iter()
            .filter(|call| call.tool == "dataframe.explain")
            .count(),
        2
    );
    let calls = recorded.calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        5,
        "only the uncommitted Repeatable attempt executes again"
    );
    let ids = recorded.ids.lock().unwrap();
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        4,
        "exactly two explain performs and two native-run performs"
    );
    assert_eq!(ids[3], ids[4], "recovery retains the logical call identity");
    assert_eq!(calls[3], calls[4], "recovery retains immutable input");
    for call in calls.iter() {
        data_only(call);
        assert_eq!(call["plan"].as_array().unwrap().len(), 2);
    }
}

fn evaluator_contract() {
    let batch = json!({"rows":[["x","9007199254740993"],["x","1"]],"schema":["value","n:int64_text"],
        "plan":[{"op":"aggregate","keys":[{"op":"col","args":["value"],"name":"value"}],
          "aggregates":[{"op":"alias","args":[{"op":"sum","args":[{"op":"col","args":["n"],"name":"n"}],"name":"sum"}],"name":"total"}]}]});
    let answer = runner::Runner::evaluate("dataframe.run", batch.clone(), || false).unwrap();
    assert_eq!(
        answer,
        json!({"rows":[["x","9007199254740994"]],"schema":["value","total:int64_text"]})
    );
    assert!(matches!(
        runner::Runner::evaluate("dataframe.run", batch, || true),
        Err(evaluator::Failure::Cancelled)
    ));
    assert!(matches!(
        runner::Runner::evaluate("dataframe.run", json!({}), || false),
        Err(evaluator::Failure::InvalidPlan(_))
    ));
    let mut overflow = json!({"rows":[["9223372036854775807"],["1"]],"schema":["n:int64_text"],
        "plan":[{"op":"aggregate","keys":[],"aggregates":[{"op":"sum","name":"n","args":[{"op":"col","args":["n"],"name":"n"}]}]}]});
    assert!(matches!(
        runner::Runner::evaluate("dataframe.run", overflow.clone(), || false),
        Err(evaluator::Failure::Overflow)
    ));
    overflow["rows"] = json!([["10"], ["2"], ["1"]]);
    overflow["plan"] = json!([{"op":"sort","keys":[{"op":"col","name":"n","args":["n"]}]}]);
    assert_eq!(
        runner::Runner::evaluate("dataframe.run", overflow, || false).unwrap()["rows"],
        json!([["1"], ["2"], ["10"]])
    );
    assert!(runner::definitions().unwrap().iter().all(|t| matches!(
        t.manifest.execution_policy,
        ExecutionPolicy::Repeatable { .. }
    )));
}

async fn publication(backend: lash::Backend) {
    let (core, creator) = host::build(
        backend,
        lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("dataframe-host"),
            lash::persistence::LeaseIncarnationId::new("publication"),
        ),
    )
    .unwrap();
    for (source, language, entry) in [
        (
            TYPESCRIPT,
            lash::workflow::document::SourceLanguage::TypeScript,
            "viaExprs",
        ),
        (
            PYTHON,
            lash::workflow::document::SourceLanguage::Python,
            "via_exprs",
        ),
    ] {
        let pin = lash::process::HostArtifactPin::mint();
        let published = creator.create(source, language, entry, &pin).await.unwrap();
        let bytes = core
            .backend()
            .definition_store()
            .get_process_definition(&published.definition.id)
            .await
            .unwrap()
            .unwrap();
        let descriptor = lash::process::ProcessDefinitionDraft::from_store_bytes(
            &published.definition.id,
            &bytes,
        )
        .unwrap();
        let declared: lash::package::PackageSelection =
            serde_json::from_value(descriptor.value().as_json()["declared_packages"].clone())
                .unwrap();
        let path = lash::package::PackagePath::parse("dataframe").unwrap();
        assert_eq!(
            declared[&path]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["run"]
        );
        creator.artifacts.release(pin).await.unwrap();
    }
}
