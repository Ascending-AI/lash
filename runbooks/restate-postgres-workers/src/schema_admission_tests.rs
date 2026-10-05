use async_trait::async_trait;
use lash::provider::LlmTransportError;
use lash::provider::ProviderHandle;
use lash::schema::ProviderSchemaCapabilities;
use lash::sync::MutexExt;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolCall, ToolDefinition,
    ToolOutcome,
};
use lash::{LashCore, TurnInput};
use lash::{openai::OpenAiCompat, openai::OpenAiCompatibleProvider};
use lash_llm_transport::{LlmHttpBody, LlmHttpRequest, LlmHttpTransport};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
const TOOL_NAME: &str = "strict_omission_probe";
const DEFAULT_LIMIT: usize = 37;
const SEED: u64 = 0x5711_c7e5;
#[derive(Clone, Copy)]
enum Endpoint {
    Chat,
}
impl Endpoint {
    fn label(self) -> &'static str {
        "chat"
    }
}
#[derive(Debug)]
struct CapturedCall {
    args: Value,
    limit: u64,
}
struct OmissionProbe {
    seen: Arc<Mutex<Vec<CapturedCall>>>,
}
#[async_trait]
impl StaticToolExecute for OmissionProbe {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        let limit = call
            .args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_LIMIT as u64);
        self.seen.lock_recover().push(CapturedCall {
            args: call.args.clone(),
            limit,
        });
        ToolOutcome::ok(json!({"limit":limit})).into()
    }
}
#[derive(Debug)]
struct CapturingScriptedTransport {
    responses: Mutex<VecDeque<String>>,
    requests: Mutex<Vec<LlmHttpRequest>>,
}

impl CapturingScriptedTransport {
    fn new(responses: impl IntoIterator<Item = String>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_bodies(&self) -> Vec<Value> {
        self.requests
            .lock_recover()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).expect("JSON request body"))
            .collect()
    }
}

#[async_trait]
impl LlmHttpTransport for CapturingScriptedTransport {
    async fn send(
        &self,
        request: LlmHttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
        self.requests.lock_recover().push(request);
        let body = self
            .responses
            .lock_recover()
            .pop_front()
            .expect("scripted response");
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: LlmHttpBody::buffered(body),
        })
    }
}

fn tool_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "required_name": { "type": "string" },
            "limit": { "type": "integer", "minimum": 1 },
            "nullable_note": { "type": ["string", "null"] },
            "nullable_referenced": { "$ref": "#/$defs/Nullable" },
            "nested": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "optional_count": { "type": "integer" }
                },
                "required": ["id"],
                "additionalProperties": false
            },
            "rows": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "optional_count": { "type": "integer" }
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }
            },
            "referenced": { "$ref": "#/$defs/Referenced" }
        },
        "required": ["required_name", "nested", "rows", "referenced"],
        "additionalProperties": false,
        "$defs": {
            "Nullable": { "type": ["string", "null"] },
            "Referenced": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "optional_count": { "type": "integer" }
                },
                "required": ["id"],
                "additionalProperties": false
            }
        }
    })
}
fn tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:strict_omission_probe",
        TOOL_NAME,
        "Capture strict omission behavior.",
        tool_input_schema(),
        json!({
            "type": "object",
            "properties": { "limit": { "type": ["integer", "null"] } },
            "required": ["limit"],
            "additionalProperties": false
        }),
    )
    .expect("valid probe schema")
}
fn core_with_backend(
    provider: ProviderHandle,
    seen: Arc<Mutex<Vec<CapturedCall>>>,
    label: &str,
    backend: lash::Backend,
) -> LashCore {
    LashCore::standard_builder(backend)
        .llm_profiles(std::sync::Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    "gpt-5.4",
                    lash::RegisteredLlmProfile::new(
                        lash::LlmProfileMetadata::builder("gpt-5.4")
                            .context_window_tokens(16_000)
                            .build()
                            .expect("valid model spec"),
                        provider,
                    ),
                )
                .expect("register the test model"),
        ))
        .tools(Arc::new(StaticToolProvider::new(
            vec![tool_definition()],
            OmissionProbe { seen },
        )))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            format!("strict-omission-{label}"),
            format!("strict-omission-{label}-boot"),
        ))
        .expect("core")
}
fn tool_response(endpoint: Endpoint, arguments: &Value) -> String {
    let arguments = serde_json::to_string(arguments).expect("arguments JSON");
    match endpoint {
        Endpoint::Chat => json!({
            "id": "chat-tool",
            "model": "gpt-5.4",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": { "name": TOOL_NAME, "arguments": arguments }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
        .to_string(),
    }
}
fn final_response(endpoint: Endpoint) -> String {
    match endpoint {
        Endpoint::Chat => json!({
            "id": "chat-final",
            "model": "gpt-5.4",
            "choices": [{
                "message": { "role": "assistant", "content": "done" },
                "finish_reason": "stop"
            }]
        })
        .to_string(),
    }
}
fn strict_arguments() -> Value {
    json!({
        "required_name": "ok",
        "limit": null,
        "nullable_note": null,
        "nullable_referenced": null,
        "nested": { "id": "nested", "optional_count": null },
        "rows": [
            { "id": "first", "optional_count": null },
            { "id": "second", "optional_count": 9 }
        ],
        "referenced": { "id": "ref", "optional_count": null }
    })
}
fn canonical_arguments() -> Value {
    json!({
        "required_name": "ok",
        "nullable_note": null,
        "nullable_referenced": null,
        "nested": { "id": "nested" },
        "rows": [{ "id": "first" }, { "id": "second", "optional_count": 9 }],
        "referenced": { "id": "ref" }
    })
}

fn dialect_only_provider(
    endpoint: Endpoint,
    transport: Arc<CapturingScriptedTransport>,
) -> ProviderHandle {
    let compat = OpenAiCompat {
        schema_capabilities: Some(ProviderSchemaCapabilities::openai(true)),
        ..OpenAiCompat::default()
    };
    match endpoint {
        Endpoint::Chat => ProviderHandle::new(
            OpenAiCompatibleProvider::new("key", "https://openai.test/v1")
                .with_compat(compat)
                .with_transport(transport)
                .into_components(),
        ),
    }
}

async fn dialect_store_law(backend: lash::Backend, label: &str) -> LashCore {
    let endpoint = Endpoint::Chat;
    let transport = Arc::new(CapturingScriptedTransport::new([
        tool_response(endpoint, &strict_arguments()),
        final_response(endpoint),
    ]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = core_with_backend(
        dialect_only_provider(endpoint, Arc::clone(&transport)),
        Arc::clone(&seen),
        label,
        backend.clone(),
    );
    let session_id = format!("{label}-{}", endpoint.label());
    runtime
        .session(lash::SessionId::fixture(&session_id))
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "gpt-5.4",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .expect("create matrix session");
    let session = runtime
        .session(lash::SessionId::fixture(session_id))
        .open()
        .await
        .expect("open matrix session");
    let result = session
        .send(TurnInput::text("Call the probe."))
        .output()
        .await
        .expect("settle matrix turn");
    assert_eq!(result.assistant_message(), Some("done"));
    session.close().await.expect("close matrix session");
    let bodies = transport.request_bodies();
    let strict = match endpoint {
        Endpoint::Chat => &bodies[0]["tools"][0]["function"]["strict"],
    };
    assert_eq!(
        *strict,
        json!(true),
        "the resolved dialect owns wire strictness"
    );
    let seen = seen.lock_recover();
    assert_eq!(
        seen.len(),
        1,
        "schema-introduced nulls cannot be blamed on the caller"
    );
    assert_eq!(seen[0].args, canonical_arguments());
    assert_eq!(seen[0].limit, DEFAULT_LIMIT as u64);
    runtime
}

async fn schema_store_law(backend: lash::Backend, label: &str) -> LashCore {
    let admitted = serde_json::to_value(tool_definition().contract())
        .expect("encode admitted catalog contract");
    for field in ["input_schema", "output_schema"] {
        for schema in [
            Value::Null,
            json!(42),
            json!("schema"),
            json!([]),
            json!({"type":"not-a-type"}),
            json!({"$ref":"https://schema.test/tool.json"}),
            json!({"$ref":"#/$defs/Absent"}),
        ] {
            let mut invalid = admitted.clone();
            invalid[field]["canonical"] = schema.clone();
            assert!(
                serde_json::from_value::<lash::tools::ToolContract>(invalid).is_err(),
                "{field} admitted a schema defect: {schema}"
            );
        }
    }
    // The accepted contract then traverses the real store and server-double turn path.
    dialect_store_law(backend, label).await
}

#[tokio::test]
async fn schema_admission_store_law_sqlite() {
    let backend = lash_restate_test::backend(SEED, Default::default())
        .await
        .expect("SQLite double");
    let _core = schema_store_law(backend.lash_backend(), "schema-sqlite").await;
}

#[tokio::test]
async fn resolved_tool_dialect_store_law_sqlite() {
    let backend = lash_restate_test::backend(SEED, Default::default())
        .await
        .expect("SQLite double");
    let _core = dialect_store_law(backend.lash_backend(), "dialect-sqlite").await;
}

async fn postgres_schema_backend() -> (
    lash::postgres::testing::IsolatedDatabase,
    tempfile::TempDir,
    lash_restate_test::RestateTestBackend,
) {
    let database = lash::postgres::testing::IsolatedDatabase::create(
        &lash::postgres::testing::required_database_url(),
    )
    .await;
    let storage = lash::postgres::PostgresStorage::connect(database.url())
        .await
        .expect("open PostgreSQL stores");
    let attachments = tempfile::tempdir().expect("attachment bytes");
    let stores: Arc<dyn lash::StoreSet> = Arc::new(lash::postgres::PostgresStoreSet::new(
        &storage,
        Arc::new(lash::persistence::FileAttachmentStore::new(
            attachments.path(),
        )),
    ));
    let backend = lash_restate_test::backend_with(SEED, Default::default(), move |_| stores)
        .await
        .expect("PostgreSQL double");
    (database, attachments, backend)
}

#[tokio::test]
#[ignore = "requires a private PostgreSQL service"]
async fn schema_admission_store_law_postgres() {
    let (_database, _attachments, backend) = postgres_schema_backend().await;
    let _core = schema_store_law(backend.lash_backend(), "schema-postgres").await;
}

#[tokio::test]
#[ignore = "requires a private PostgreSQL service"]
async fn resolved_tool_dialect_store_law_postgres() {
    let (_database, _attachments, backend) = postgres_schema_backend().await;
    let _core = dialect_store_law(backend.lash_backend(), "dialect-postgres").await;
}

async fn sqlite_file_schema_backend() -> (
    tempfile::TempDir,
    lash_restate_test::RestateTestBackend<dyn lash::StoreSet>,
) {
    let directory = tempfile::tempdir().expect("SQLite file stores");
    let root = directory.path().to_path_buf();
    let backend = lash_restate_test::backend_with_store_set(
        SEED,
        Default::default(),
        Default::default(),
        move |clock| async move {
            let stores = lash::sqlite::SqliteStoreSet::open_with_clock(root, clock)
                .await
                .expect("open SQLite file stores");
            Ok(Arc::new(stores) as Arc<dyn lash::StoreSet>)
        },
    )
    .await
    .expect("SQLite file double");
    (directory, backend)
}

#[tokio::test]
async fn schema_admission_store_law_sqlite_file() {
    let (_directory, backend) = sqlite_file_schema_backend().await;
    let _core = schema_store_law(backend.lash_backend(), "schema-sqlite-file").await;
}

#[tokio::test]
async fn resolved_tool_dialect_store_law_sqlite_file() {
    let (_directory, backend) = sqlite_file_schema_backend().await;
    let _core = dialect_store_law(backend.lash_backend(), "dialect-sqlite-file").await;
}

async fn live_schema_backend(
    label: &str,
    stores: Arc<dyn lash::StoreSet>,
) -> lash_restate_test::live::LiveRestateBackend<dyn lash::StoreSet> {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("private endpoint port");
    let endpoint_bind = reservation.local_addr().expect("endpoint address");
    let tag = format!(
        "schema-{}-{}",
        std::env::var("KILN_GATE_ID").expect("live laws run in a private Kiln gate"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("wall clock")
            .as_nanos()
    );
    let config = lash_restate_test::live::LiveConfig {
        ingress_url: std::env::var("RESTATE_INGRESS_URL").expect("private Restate ingress"),
        admin_url: std::env::var("RESTATE_ADMIN_URL").expect("private Restate admin"),
        endpoint_bind,
        endpoint_url: format!("http://{endpoint_bind}"),
        run_tag: format!("{tag}-{label}"),
        namespace: lash::restate::RestateNamespace::new(tag).expect("private schema namespace"),
    };
    drop(reservation);
    lash_restate_test::live::LiveRestateBackend::start_with_store_set(config, |_| async {
        Ok(stores)
    })
    .await
    .expect("live Restate backend")
}

#[tokio::test]
#[ignore = "requires private live Restate"]
async fn schema_admission_store_law_live_restate_sqlite_file() {
    let directory = tempfile::tempdir().expect("SQLite file stores");
    let stores = lash::sqlite::SqliteStoreSet::open(directory.path())
        .await
        .expect("open SQLite file stores");
    let backend = live_schema_backend("admission-sqlite", Arc::new(stores)).await;
    let _core = schema_store_law(backend.lash_backend(), "schema-live-sqlite").await;
    backend.finish().await;
}

#[tokio::test]
#[ignore = "requires private live Restate"]
async fn resolved_tool_dialect_store_law_live_restate_sqlite_file() {
    let directory = tempfile::tempdir().expect("SQLite file stores");
    let stores = lash::sqlite::SqliteStoreSet::open(directory.path())
        .await
        .expect("open SQLite file stores");
    let backend = live_schema_backend("dialect-sqlite", Arc::new(stores)).await;
    let _core = dialect_store_law(backend.lash_backend(), "dialect-live-sqlite").await;
    backend.finish().await;
}

async fn live_postgres_schema_backend(
    label: &str,
) -> (
    lash::postgres::testing::IsolatedDatabase,
    tempfile::TempDir,
    lash_restate_test::live::LiveRestateBackend<dyn lash::StoreSet>,
) {
    let database = lash::postgres::testing::IsolatedDatabase::create(
        &lash::postgres::testing::required_database_url(),
    )
    .await;
    let storage = lash::postgres::PostgresStorage::connect(database.url())
        .await
        .expect("open PostgreSQL stores");
    let attachments = tempfile::tempdir().expect("attachment bytes");
    let stores = Arc::new(lash::postgres::PostgresStoreSet::new(
        &storage,
        Arc::new(lash::persistence::FileAttachmentStore::new(
            attachments.path(),
        )),
    ));
    let backend = live_schema_backend(label, stores).await;
    (database, attachments, backend)
}

#[tokio::test]
#[ignore = "requires PostgreSQL and private live Restate"]
async fn schema_admission_store_law_live_restate_postgres() {
    let (_database, _attachments, backend) = live_postgres_schema_backend("admission-pg").await;
    let _core = schema_store_law(backend.lash_backend(), "schema-live-postgres").await;
    backend.finish().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL and private live Restate"]
async fn resolved_tool_dialect_store_law_live_restate_postgres() {
    let (_database, _attachments, backend) = live_postgres_schema_backend("dialect-pg").await;
    let _core = dialect_store_law(backend.lash_backend(), "dialect-live-postgres").await;
    backend.finish().await;
}

#[test]
fn schema_admission_causes_survive_plugin_host_and_remote_shapes() {
    let source = lash::schema::JsonSchema::admit(Value::Null).expect_err("null is not a schema");
    let plugin = lash::plugins::PluginError::UnusableSchema {
        source: Box::new(source.clone()),
    };
    let plugin: lash::plugins::PluginError =
        serde_json::from_value(serde_json::to_value(plugin).expect("encode plugin error"))
            .expect("decode plugin error");
    let runtime = plugin.into_turn_failure(lash::runtime::RuntimeErrorCode::PluginSessionManager);
    assert!(runtime.is_terminal());
    let runtime: lash::runtime::RuntimeError =
        serde_json::from_value(serde_json::to_value(runtime).expect("encode runtime error"))
            .expect("decode runtime error");
    assert!(
        matches!(runtime.cause, Some(lash::runtime::RuntimeErrorCause::SchemaRefused { source: retained }) if *retained == source)
    );
    let host = lash::rlm::lang::ExecutionHostError::from_schema_admission(source.clone());
    let host: lash::rlm::lang::ExecutionHostError =
        serde_json::from_value(serde_json::to_value(host).expect("encode host error"))
            .expect("decode host error");
    assert_eq!(host.schema_admission(), Some(&source));
    let catalog = ToolDefinition::raw("bad", "bad", "bad", Value::Null, json!({}))
        .expect_err("unusable catalog member");
    for cause in [
        lash::tools::ToolFailureCause::SchemaAdmission {
            source: source.clone(),
        },
        lash::tools::ToolFailureCause::ToolSchemaAdmission {
            source: Box::new(catalog.clone()),
        },
        lash::tools::ToolFailureCause::ValueMismatch {
            source: lash::schema::ValueMismatch {
                instance_path: "/count".into(),
                message: "integer required".into(),
            },
        },
    ] {
        let failure = lash::tools::ToolFailure::tool(
            lash::tools::ToolFailureClass::Internal,
            "schema",
            "schema refusal",
        )
        .with_cause(cause.clone());
        let host = lash::rlm::lang::ExecutionHostError::from_tool_failure(&failure, "schema-call");
        let host: lash::rlm::lang::ExecutionHostError =
            serde_json::from_value(serde_json::to_value(host).expect("encode tool host error"))
                .expect("decode tool host error");
        let mut conflicting = serde_json::to_value(&host).expect("encode tool host error");
        conflicting["schema_admission"] =
            serde_json::to_value(&source).expect("encode schema cause");
        assert!(
            serde_json::from_value::<lash::rlm::lang::ExecutionHostError>(conflicting).is_err()
        );
        assert_eq!(
            host.tool_failure()
                .expect("typed tool failure")
                .cause
                .as_deref(),
            Some(&cause)
        );
        let remote = lash::remote::turn_result::RemoteToolFailure::from(failure.clone());
        let remote: lash::remote::turn_result::RemoteToolFailure = serde_json::from_value(
            serde_json::to_value(remote).expect("encode shared remote failure"),
        )
        .expect("decode shared remote failure");
        assert_eq!(remote.cause.as_deref(), Some(&cause));
        let output = lash::tools::ToolCallOutput::failure(failure);
        let remote = lash::remote::processes::RemoteProcessToolCallOutput::try_from(output)
            .expect("encode remote output");
        let remote: lash::remote::processes::RemoteProcessToolCallOutput =
            serde_json::from_value(serde_json::to_value(remote).expect("encode wire output"))
                .expect("decode wire output");
        let output = lash::tools::ToolCallOutput::try_from(remote).expect("restore output");
        let lash::tools::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("failure remains a failure")
        };
        assert_eq!(failure.cause.as_deref(), Some(&cause));
    }
    let mismatch = lash::schema::ValueMismatch {
        instance_path: "/count".into(),
        message: "integer required".into(),
    };
    for (plugin, expected) in [
        (
            lash::plugins::PluginError::UnusableSchema {
                source: Box::new(source.clone()),
            },
            lash::runtime::RuntimeErrorCause::SchemaRefused {
                source: Box::new(source.clone()),
            },
        ),
        (
            lash::plugins::PluginError::UnusableToolSchema {
                source: Box::new(catalog.clone()),
            },
            lash::runtime::RuntimeErrorCause::ToolSchemaRefused {
                source: Box::new(catalog),
            },
        ),
        (
            lash::plugins::PluginError::ValueMismatch {
                context: "payload".into(),
                source: Box::new(mismatch.clone()),
            },
            lash::runtime::RuntimeErrorCause::ValueMismatch {
                context: "payload".into(),
                source: Box::new(mismatch.clone()),
            },
        ),
    ] {
        let runtime =
            lash::runtime::RuntimeEffectControllerError::from(plugin).into_runtime_error();
        let runtime: lash::runtime::RuntimeError = serde_json::from_value(
            serde_json::to_value(runtime).expect("encode controller refusal"),
        )
        .expect("decode controller refusal");
        assert!(runtime.is_terminal());
        assert!(!runtime.is_retryable());
        assert_eq!(runtime.cause, Some(expected));
    }
    let delivery = lash::triggers::TriggerDeliveryEmitOutcome::Failed {
        code: lash::runtime::RuntimeErrorCode::PluginSessionManager,
        reason: "value mismatch".into(),
        value_mismatch: Some(Box::new(mismatch.clone())),
    };
    let remote = lash::remote::triggers::RemoteTriggerDeliveryEmitOutcome::from(delivery.clone());
    let remote: lash::remote::triggers::RemoteTriggerDeliveryEmitOutcome =
        serde_json::from_value(serde_json::to_value(remote).expect("encode remote delivery"))
            .expect("decode remote delivery");
    assert_eq!(
        lash::triggers::TriggerDeliveryEmitOutcome::from(remote),
        delivery
    );
    for cell in [
        lash::plugins::CellFailure::new(lash::plugins::CellFailureKind::Host, "schema refusal")
            .with_schema_admission(source.clone()),
        lash::plugins::CellFailure::new(lash::plugins::CellFailureKind::Program, "value mismatch")
            .with_value_mismatch(mismatch.clone()),
    ] {
        let remote = lash::remote::usage::RemoteTurnEvent::CodeBlockCompleted {
            language: "lashlang".into(),
            output: String::new(),
            error: Some(cell.clone()),
            duration_ms: 0,
            tool_call_ids: Vec::new(),
            graph_key: None,
        };
        let remote: lash::remote::usage::RemoteTurnEvent =
            serde_json::from_value(serde_json::to_value(remote).expect("encode remote cell"))
                .expect("decode remote cell");
        let lash::remote::usage::RemoteTurnEvent::CodeBlockCompleted {
            error: Some(retained),
            ..
        } = remote
        else {
            panic!("typed cell failure remains a cell failure")
        };
        assert_eq!(retained, cell);
    }
    let refusal = lash::rlm::RunRefusal::UnusableSchema {
        source: Box::new(source.clone()),
    };
    let outcome = lash::rlm::InfrastructureOutcome::from(refusal);
    assert!(!outcome.is_retryable());
    let outcome: lash::rlm::InfrastructureOutcome =
        serde_json::from_value(serde_json::to_value(outcome).expect("encode worker refusal"))
            .expect("decode worker refusal");
    assert!(
        matches!(outcome, lash::rlm::InfrastructureOutcome::RunRefused {
        refusal: lash::rlm::RunRefusal::UnusableSchema { source: retained }
    } if *retained == source)
    );
}
