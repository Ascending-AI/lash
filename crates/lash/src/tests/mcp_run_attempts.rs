use std::collections::BTreeMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::ToolProvider;
use lash_core::plugin::{BehaviorRevision, PluginRevision};
use lash_core::store::plugin_writers::PluginCallbackIdentity;
use lash_core::tool_dispatch::{
    BeforeCheckReply, DeclaredStartObligation, SingletonAttempt, SingletonBodyOutcome,
    SingletonCapture, SingletonPreparedRequest, SingletonTerminal, SingletonToolCall,
    SingletonToolHandlers, run_singleton_tool,
};
use lash_core::tool_run::{
    AdmittedBinding, AfterCheckVerdict, AttributedVerdict, CallDecision, ExternalCancelPolicy,
    PresentationBinding, SegmentOrdinal,
};
use lash_core::{AdmittedScope, EffectOpener, ToolCallId, ToolManifest};
use lash_restate_test::{CrashPoint, CrashRule, ServerConfig};
use serde_json::{Value, json};

const PEER: &str = r#"
import json, os, sys
seen = set()
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'initialize':
        result = {'protocolVersion': '2025-11-25', 'capabilities': {'tools': {}},
                  'serverInfo': {'name': 'attempt-law-peer', 'version': '1'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': 'work', 'inputSchema': {'type': 'object'}}]}
    elif method == 'tools/call':
        meta = request['params'].get('_meta', {})
        call_id = meta.get('lash.dev/tool-call-id')
        with open(os.environ['TRACE'], 'a') as trace:
            trace.write(json.dumps(request) + '\n')
        if os.environ['DEDUP'] == 'false' or call_id not in seen:
            with open(os.environ['EFFECTS'], 'a') as effects:
                effects.write(json.dumps(call_id) + '\n')
            seen.add(call_id)
        result = {'content': [{'type': 'text', 'text': 'done'}]}
    else:
        continue
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
"#;

struct Probe {
    provider: crate::mcp::McpToolProvider,
    manifest: ToolManifest,
    cancel: AtomicBool,
    cancel_before_decision: bool,
    presentations: AtomicUsize,
}

#[async_trait::async_trait]
impl SingletonToolHandlers for Probe {
    async fn prepare(&self, call: &SingletonToolCall) -> Result<Value, String> {
        let context = lash_core::ToolPrepareContext::for_testing(
            lash_core::RuntimeOwner::Session("mcp-run".into()),
            Arc::new(lash_core::testing::MockSessionManager::default()),
            None,
        );
        let prepared = self
            .provider
            .prepare_tool_call(lash_core::ToolPrepareCall {
                tool_id: self.manifest.id.clone(),
                pending: lash_core::sansio::PendingToolCall {
                    call_id: call.call_id.clone(),
                    provider_call_id: None,
                    tool_name: self.manifest.name.clone(),
                    args: call.arguments.clone(),
                    replay: None,
                },
                context: &context,
            })
            .await
            .map_err(|failure| format!("{failure:?}"))?;
        Ok(json!({"manifest": self.manifest, "call": prepared}))
    }

    async fn before_checks(
        &self,
        _: &SingletonToolCall,
        _: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        Ok(Vec::new())
    }

    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        let manifest: ToolManifest =
            serde_json::from_value(attempt.request.prepared["manifest"].clone()).unwrap();
        let prepared: lash_core::PreparedToolCall =
            serde_json::from_value(attempt.request.prepared["call"].clone()).unwrap();
        assert_eq!(prepared.call_id, *attempt.call_id);
        let fixture = lash_core::testing::ToolCallFixture::mock().prepared_call(&prepared);
        let context = fixture.attempt("mcp-run-law");
        assert_eq!(attempt.attempt.get(), context.attempt_number());
        let result = self
            .provider
            .execute(lash_core::ToolCall::new(
                &manifest,
                &attempt.request.arguments,
                &context,
            ))
            .await;
        let lash_core::ToolAttemptOutcome::Done { result, intents } = result else {
            panic!("a socket cannot return independent durable work");
        };
        assert!(intents.is_empty());
        if self.cancel_before_decision {
            self.cancel.store(true, Ordering::SeqCst);
        }
        let output = result.into_output();
        let text = serde_json::to_string(&output).unwrap();
        Ok(if output.is_success() {
            SingletonBodyOutcome::Done {
                output: text,
                commands: Default::default(),
                intents: Vec::new(),
                start: None,
            }
        } else {
            SingletonBodyOutcome::Failed { output: text }
        })
    }

    async fn after_checks(
        &self,
        _: &ToolCallId,
        _: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        Ok(Vec::new())
    }

    async fn run_cancel_requested(&self) -> Result<bool, String> {
        Ok(self.cancel.load(Ordering::SeqCst))
    }
    async fn wait_run_retry(
        &self,
        timer: lash_core::tool_dispatch::RunRetryTimer<'_>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        timer.await
    }

    async fn realize_declarations(
        &self,
        _: &ToolCallId,
        _: &[lash_sansio::ToolIntentKind],
    ) -> Result<(), String> {
        panic!("inline MCP declares no Lash effects")
    }

    async fn present(
        &self,
        _: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, lash_core::tool_dispatch::SingletonPresentationError> {
        self.presentations.fetch_add(1, Ordering::SeqCst);
        self.cancel.store(true, Ordering::SeqCst);
        Ok(capture.output().unwrap().to_string())
    }

    fn emit_stream(&self, _: &ToolCallId, _: &lash_core::runtime::AttemptStream) {}

    async fn launch_start(
        &self,
        _: &DeclaredStartObligation,
    ) -> Result<lash_core::ProcessId, String> {
        panic!("an MCP socket is no process implementation")
    }

    async fn discharge_start(
        &self,
        _: &DeclaredStartObligation,
        _: &lash_core::ProcessId,
        _: bool,
    ) -> Result<(), String> {
        panic!("inline MCP owns no process hold")
    }
}

fn call(manifest: &ToolManifest, label: &str) -> SingletonToolCall {
    let revision = PluginRevision::new("mcp", BehaviorRevision::new(1).unwrap());
    let callback = PluginCallbackIdentity {
        owner: revision.clone(),
        key: manifest.id.to_string(),
    };
    SingletonToolCall {
        owner: EffectOpener::turn("mcp-run", "turn"),
        segment: SegmentOrdinal(0),
        call_id: ToolCallId::fixture(label),
        tool_name: manifest.name.clone(),
        arguments: json!({}),
        declaration: manifest.declaration.clone(),
        binding: AdmittedBinding {
            executable: callback.clone(),
            preparation: callback,
            presentation: PresentationBinding {
                presenter: None,
                steps: Vec::new(),
            },
        },
        available: vec![revision],
        cancel: ExternalCancelPolicy::Ignore,
        environment: None,
    }
}

async fn pool(
    trace: &std::path::Path,
    effects: &std::path::Path,
    dedup: bool,
) -> Arc<crate::mcp::McpConnectionPool> {
    let config = crate::mcp::McpServerConfig::stdio(
        crate::mcp::McpStdioTransport::new("python3", vec!["-u".into(), "-c".into(), PEER.into()])
            .with_env(BTreeMap::from([
                ("TRACE", trace.display().to_string()),
                ("EFFECTS", effects.display().to_string()),
                ("DEDUP", dedup.to_string()),
            ])),
    );
    let pool = crate::mcp::McpConnectionPool::connect(BTreeMap::from([("fixture".into(), config)]))
        .await
        .unwrap();
    assert_eq!(pool.advertised_tools().len(), 1);
    pool
}

async fn drive(
    probe: Arc<Probe>,
    call: SingletonToolCall,
    crash: Option<&str>,
) -> SingletonTerminal {
    let backend = lash_restate_test::backend(0x4886, ServerConfig::default())
        .await
        .unwrap();
    if let Some(step) = crash {
        backend
            .server()
            .crash_on(CrashRule::new(CrashPoint::BeforeRunResult {
                name: Some(format!("lash:run:{}:{step}", call.call_id)),
            }));
    }
    let returned = Arc::new(Mutex::new(None));
    let handler: lash_restate_test::HandlerAttempt = {
        let returned = returned.clone();
        Arc::new(move |scoped| {
            let probe = probe.clone();
            let call = call.clone();
            let returned = returned.clone();
            Box::pin(async move {
                let outcome = run_singleton_tool(&scoped, &call, probe.as_ref())
                    .await
                    .unwrap();
                *returned.lock().unwrap() = Some(outcome.terminal);
            })
        })
    };
    backend
        .run_in_handler(AdmittedScope::turn("mcp-run", "turn"), handler)
        .await
        .unwrap();
    returned.lock().unwrap().take().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l02_mcp_replays_only_unrecorded_attempts_and_remote_dedup_is_explicit() {
    for dedup in [false, true] {
        for cut in [
            None,
            Some("admit"),
            Some("attempt:1"),
            Some("decide"),
            Some("present"),
        ] {
            let mut trace = tempfile::NamedTempFile::new().unwrap();
            let mut effects = tempfile::NamedTempFile::new().unwrap();
            let pool = pool(trace.path(), effects.path(), dedup).await;
            let provider = crate::mcp::McpToolProvider::new(pool.clone());
            let manifest = provider.tool_manifests().remove(0);
            let call = call(&manifest, "mcp-crash-window");
            let call_id = call.call_id.clone();
            let probe = Arc::new(Probe {
                provider,
                manifest,
                cancel: AtomicBool::new(false),
                cancel_before_decision: false,
                presentations: AtomicUsize::new(0),
            });
            let terminal = drive(probe, call, cut).await;
            assert!(matches!(terminal, SingletonTerminal::Final { .. }));
            pool.shutdown_all().await;
            let mut requests = String::new();
            trace.read_to_string(&mut requests).unwrap();
            let requests = requests
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            let deliveries = 1 + usize::from(cut == Some("attempt:1"));
            assert_eq!(requests.len(), deliveries, "cut={cut:?}");
            for request in &requests {
                assert_eq!(
                    request["params"]["_meta"]["lash.dev/tool-call-id"],
                    call_id.to_string()
                );
                assert_eq!(request["params"]["_meta"]["lash.dev/tool-attempt"], 1);
                assert_eq!(request["params"]["name"], "work");
            }
            if requests.len() == 2 {
                assert_ne!(requests[0]["id"], requests[1]["id"]);
            }
            let mut effects_text = String::new();
            effects.read_to_string(&mut effects_text).unwrap();
            let effects = effects_text.lines().count();
            assert_eq!(
                effects,
                if dedup { 1 } else { deliveries },
                "fixture dedup={dedup}, cut={cut:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l03_mcp_run_cancellation_withholds_undecided_results_and_preserves_finals() {
    for cancel_before_decision in [false, true] {
        let mut trace = tempfile::NamedTempFile::new().unwrap();
        let effects = tempfile::NamedTempFile::new().unwrap();
        let pool = pool(trace.path(), effects.path(), false).await;
        let provider = crate::mcp::McpToolProvider::new(pool.clone());
        let manifest = provider.tool_manifests().remove(0);
        let call = call(&manifest, "mcp-final-or-cancel");
        let probe = Arc::new(Probe {
            provider,
            manifest,
            cancel: AtomicBool::new(false),
            cancel_before_decision,
            presentations: AtomicUsize::new(0),
        });
        let terminal = drive(probe.clone(), call, Some("present")).await;
        if cancel_before_decision {
            assert_eq!(
                terminal,
                SingletonTerminal::Withheld {
                    decision: CallDecision::Cancelled
                }
            );
            assert_eq!(probe.presentations.load(Ordering::SeqCst), 0);
        } else {
            assert!(matches!(terminal, SingletonTerminal::Final { .. }));
            assert_eq!(
                probe.presentations.load(Ordering::SeqCst),
                2,
                "lost presentation reruns, durable X does not"
            );
        }
        pool.shutdown_all().await;
        let mut requests = String::new();
        trace.read_to_string(&mut requests).unwrap();
        assert_eq!(requests.lines().count(), 1);
    }
}
