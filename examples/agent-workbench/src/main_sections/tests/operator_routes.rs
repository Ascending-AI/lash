use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

struct OperatorHarness {
    double: lash_restate_test::RestateTestBackend,
    state: AppState,
    base: String,
    server: tokio::task::JoinHandle<()>,
    repaired: Arc<AtomicBool>,
}
impl Drop for OperatorHarness {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl OperatorHarness {
    async fn new(standard: bool, faulty: bool) -> Self {
        let double = test_double_backend(4701).await;
        let repaired = Arc::new(AtomicBool::new(!faulty));
        let provider = lash::testing::TestProvider::builder()
            .kind("operator-routes")
            .complete(move |_| async move {
                Ok(text_response(if standard {
                    "operator answer"
                } else {
                    "<typescript>finish(\"operator answer\");</typescript>"
                }))
            })
            .build()
            .into_handle();
        let builder = if standard {
            LashCore::standard_builder(double.lash_backend())
                .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        } else {
            explicit_durable_test_facets_on(double.lash_backend())
        };
        let core = builder
            .serve_workbench_llm_profile(provider, test_llm_profile())
            .plugin(Arc::new(OperatorFault {
                repaired: repaired.clone(),
            }))
            .build(crate::test_core_owner())
            .unwrap();
        install_test_process_worker(&double, &core);
        let state = AppState {
            session_defaults: test_session_defaults(),
            attachment_store: test_attachment_store(),
            session_store_factory: double.stores().session_store_factory(),
            trigger_store: detached_trigger_store(),
            process_observer: core.processes().observer().unwrap(),
            core,
            sessions: WorkbenchSessions::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_llm_profile: Arc::new(Mutex::new(LlmProfileSelection {
                model: "test-model".into(),
                model_variant: None,
            })),
            trace_sink: None,
            lashlang_execution: Arc::default(),
            event_tx: SessionEventRegistry::new(16),
            restate_ingress_url: "http://127.0.0.1:8080".into(),
            restate_admin_url: "http://127.0.0.1:9070".into(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::default(),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            unknown_turn_terminals: UnknownTurnTerminals::default(),
            authorization: WorkbenchAuthorization::allow_all(),
            approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
        };
        if standard {
            state
                .core
                .session(state.current_session_id())
                .create(lash::SessionCreation::root(test_session_defaults()))
                .await
                .unwrap();
        } else {
            state.ensure_current_session().await.unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = operator_routes().with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            double,
            state,
            base,
            server,
            repaired,
        }
    }
    fn session_path(&self, suffix: &str) -> String {
        format!(
            "/api/admin/sessions/{}{suffix}",
            self.state.current_session_id()
        )
    }
    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request =
            reqwest::Client::new().request(method.parse().unwrap(), format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        let body = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{method} {path}: {status}: {text}: {error}"));
        (status, body)
    }
    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self.request(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status}: {body}");
        body
    }
    async fn send(&self, id: &'static str) -> lash::SendOutcome {
        self.state
            .core
            .session(self.state.current_session_id())
            .durable()
            .await
            .unwrap()
            .send(lash::TurnInput::text("operator test"))
            .id(id)
            .await
            .unwrap()
            .outcome()
            .await
            .unwrap()
    }
    async fn park(&self) -> Value {
        let outcome = self.send("operator-park").await;
        assert!(
            matches!(outcome.status(), lash::TurnStatus::Parked(_)),
            "{outcome:?}"
        );
        let page = self.ok("GET", "/api/admin/parks?limit=1", None).await;
        assert_eq!(page["records"].as_array().unwrap().len(), 1);
        let record = &page["records"][0];
        assert_eq!(
            record["target"]["session_id"],
            self.state.current_session_id().as_str()
        );
        json!({"target": record["target"], "park_id": record["park_id"]})
    }
}

struct OperatorFault {
    repaired: Arc<AtomicBool>,
}
impl PluginFactory for OperatorFault {
    fn id(&self) -> &'static str {
        "operator-test-fault"
    }
    fn declaration(&self) -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(PluginFactory::id(self))
    }
    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(Self {
            repaired: self.repaired.clone(),
        }))
    }
}
impl SessionPlugin for OperatorFault {
    fn id(&self) -> &'static str {
        "operator-test-fault"
    }
    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), PluginError> {
        let repaired = self.repaired.clone();
        registrar.output().response(
            lash::hook_key!("fault"),
            None,
            Arc::new(move |ctx| {
                let repaired = repaired.clone();
                Box::pin(async move {
                    if !repaired.load(Ordering::SeqCst) {
                        return Err(PluginError::RuntimeEffectController(
                            lash::runtime::RuntimeEffectControllerError::new(
                                lash::runtime::RuntimeErrorCode::LashlangCellReplayDivergence,
                                "operator test replay refusal",
                            ),
                        ));
                    }
                    Ok(lash::plugins::AssistantResponseTransform {
                        response: ctx.response,
                        events: Vec::new(),
                    })
                })
            }),
        )?;
        Ok(())
    }
}

#[test]
fn park_list_events_and_redrive_preserve_the_park_token() {
    run_async_test_on_stack_budget("operator-redrive", || async {
        let h = OperatorHarness::new(false, true).await;
        let park = h.park().await;
        let events = h.ok("GET", "/api/admin/parks/events", None).await;
        assert!(!events["events"].as_array().unwrap().is_empty());
        let after = serde_json::to_string(&events["next"]).unwrap();
        let (status, _) = h
            .request(
                "GET",
                &format!(
                    "/api/admin/parks/events?after={}",
                    percent_encoding::utf8_percent_encode(
                        &after,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                ),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        h.repaired.store(true, Ordering::SeqCst);
        let accepted = h.ok("POST", "/api/admin/parks/redrive", Some(park)).await;
        assert_eq!(accepted["kind"], "turn");
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let result = h
                    .state
                    .core
                    .session(h.state.current_session_id())
                    .open()
                    .await
                    .unwrap()
                    .run("operator-park")
                    .outcome()
                    .await
                    .unwrap();
                if !matches!(result.status(), lash::TurnStatus::Parked(_)) {
                    break result;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the accepted redrive resumes its retained journal");
        assert_eq!(result.status(), lash::TurnStatus::Answered);
    });
}
#[test]
fn park_cancel_settles_and_rejects_a_second_cancel() {
    run_async_test_on_stack_budget("operator-park-cancel", || async {
        let h = OperatorHarness::new(false, true).await;
        let park = h.park().await;
        let result = h
            .ok("POST", "/api/admin/parks/cancel", Some(park.clone()))
            .await;
        assert_eq!(result["applied"], true);
        assert_eq!(
            h.ok("GET", "/api/admin/parks", None).await["records"],
            json!([])
        );
        let (status, refusal) = h
            .request("POST", "/api/admin/parks/cancel", Some(park))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(refusal["cause"]["kind"], "not_parked");
    });
}
#[test]
fn park_fork_returns_the_successor_and_typed_process_refusal() {
    run_async_test_on_stack_budget("operator-park-fork", || async {
        let h = OperatorHarness::new(false, true).await;
        let park = h.park().await;
        h.repaired.store(true, Ordering::SeqCst);
        let result = h.ok("POST", "/api/admin/parks/fork", Some(park)).await;
        assert!(result["new_turn"].is_string());
        let (status, result) = h
            .request(
                "POST",
                "/api/admin/parks/fork",
                Some(
                    json!({"target": {"kind":"process","process_id": lash::ProcessId::fixture("unknown")}, "park_id":1}),
                ),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(result["cause"]["kind"], "fork_requires_turn");
    });
}
#[test]
fn deployment_and_generation_drain_report_the_actual_marks() {
    run_async_test_on_stack_budget("operator-drain", || async {
        let h = OperatorHarness::new(false, false).await;
        assert_eq!(
            h.ok("GET", "/api/admin/drain?accepting_new_work=true", None)
                .await["drained"],
            false
        );
        assert_eq!(
            h.ok("GET", "/api/admin/drain?accepting_new_work=false", None)
                .await["drained"],
            true
        );
        let path = "/api/admin/generations/aaaaaaaaaaaa/drain";
        assert_eq!(h.ok("POST", path, None).await["changed"], true);
        let status = h.ok("GET", path, None).await;
        assert!(status["draining_since_ms"].is_number());
        assert_eq!(h.ok("DELETE", path, None).await["changed"], true);
        assert_eq!(h.ok("DELETE", path, None).await["changed"], false);
        let own = format!(
            "/api/admin/generations/{}/drain",
            h.state.core.build_generation()
        );
        let (status, result) = h.request("POST", &own, None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(result["cause"]["kind"], "drain_own_generation");
    });
}
#[test]
fn stalled_delivery_listing_and_rearm_use_the_owning_ledger() {
    run_async_test_on_stack_budget("operator-rearm", || async {
        use lash::StoreSet as _;
        let h = OperatorHarness::new(false, false).await;
        let hold = h
            .double
            .hold_session_shift(&h.state.current_session_id())
            .await;
        let session = h
            .state
            .core
            .session(h.state.current_session_id())
            .durable()
            .await
            .unwrap();
        let sent = session
            .send(lash::TurnInput::text("ledger row"))
            .id("rearm-input")
            .await
            .unwrap();
        let input_id = sent.input_id().clone();
        let ledger = h
            .double
            .stores()
            .obligation_ledger(lash::ObligationKind::Ingress);
        let key = lash::ObligationKey::Ingress {
            session_id: h.state.current_session_id(),
            item_id: input_id.to_string(),
        };
        let id = key.id();
        let now = h.double.lash_backend().clock().timestamp_ms();
        let claims = ledger
            .claim_due(
                now + 120_000,
                60_000,
                std::num::NonZeroUsize::new(10).unwrap(),
            )
            .await
            .unwrap();
        let claim = claims
            .into_iter()
            .find(|claim| claim.id == id)
            .expect("the held input remains owed");
        ledger
            .settle(
                &id,
                &claim.token,
                lash::persistence::ObligationSettlement::Stall {
                    reason: lash::StallReason::Refused,
                    error: lash::DeliveryError::new(
                        lash::runtime::RuntimeErrorCode::RuntimeStoreCorrupt,
                        "operator repaired the row",
                    ),
                },
                now,
            )
            .await
            .unwrap();
        let page = h.ok("GET", "/api/admin/obligations/ingress", None).await;
        assert_eq!(page["records"][0]["id"], json!(id));
        assert_eq!(
            page["records"][0]["last_error"]["code"],
            "runtime_store_corrupt"
        );
        assert_eq!(
            h.ok(
                "POST",
                "/api/admin/obligations/rearm",
                Some(json!({"kind":"ingress", "id":id}))
            )
            .await["rearmed"],
            true
        );
        assert_eq!(
            h.ok(
                "POST",
                "/api/admin/obligations/rearm",
                Some(json!({"kind":"ingress", "id":id}))
            )
            .await["rearmed"],
            false
        );
        assert_eq!(ledger.count_stalled().await.unwrap(), 0);
        drop(hold);
        sent.outcome().await.unwrap();
    });
}
#[test]
fn usage_pages_export_real_calls_and_reconciliation_keeps_unknown_amounts_explicit() {
    run_async_test_on_stack_budget("operator-usage", || async {
        let h = OperatorHarness::new(false, false).await;
        h.send("usage-one").await;
        h.send("usage-two").await;
        let facts = h
            .ok("GET", &h.session_path("/usage/facts?limit=1"), None)
            .await;
        assert_eq!(facts["facts"].as_array().unwrap().len(), 1);
        assert!(!facts["next"].is_null());
        let after = serde_json::to_string(&facts["next"]).unwrap();
        let next = h
            .ok(
                "GET",
                &h.session_path(&format!(
                    "/usage/facts?limit=1&after={}",
                    percent_encoding::utf8_percent_encode(
                        &after,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                )),
                None,
            )
            .await;
        assert_ne!(next["facts"][0], facts["facts"][0]);
        let meters = h
            .ok("GET", &h.session_path("/usage/meters?limit=1"), None)
            .await;
        assert_eq!(meters["runs"].as_array().unwrap().len(), 1);
        let reconciliation = h
            .ok("POST", &h.session_path("/usage/reconcile"), None)
            .await;
        assert!(reconciliation.get("unresolved").is_some());
    });
}

async fn apply_config(h: &OperatorHarness, revision: u64, id: &str, commands: Value) -> Value {
    let submitted = h
        .ok(
            "POST",
            &h.session_path("/config"),
            Some(json!({"id":id,"expected_revision":revision,"commands":commands})),
        )
        .await;
    assert_eq!(submitted["kind"], "pending");
    h.ok(
        "POST",
        &h.session_path("/config/settle"),
        Some(submitted["receipt"].clone()),
    )
    .await
}
#[test]
fn recorded_core_and_rlm_config_commands_settle_and_stale_edits_publish_nothing() {
    run_async_test_on_stack_budget("operator-recorded-config", || async {
        let h = OperatorHarness::new(false, false).await;
        let before = h.ok("GET", &h.session_path("/config"), None).await;
        let revision = before["catalog"]["revision"].as_u64().unwrap();
        let commands = json!([
            {"kind":"set_llm_profile","args":{"model":"test-model"}},
            {"kind":"set_reasoning","args":{"reasoning":"provider_default"}},
            {"kind":"set_turn_budget","args":{"turn_budget":{"bounded":10}}},
            {"kind":"set_max_tool_calls","args":{"max_tool_calls":64}},
            {"kind":"set_tool_access","args":{"access":{"mode":"restricted","tools":[]}}},
            {"kind":"set_autonomy","args":{"autonomous":true}},
            {"kind":"set_charge_safety","args":{"charge_safety":{"mode":"require_guarantee"}}},
            {"kind":"set_no_progress_budget","args":{"no_progress_budget":{"bounded":3}}},
            {"kind":"set_generation","args":{"generation":{"mode":"replace","generation":{}}}},
            {"kind":"set_attachment_acceptance","args":{"acceptance":{"revision":"operator-test","acceptors":[]}}},
            {"kind":"set_rlm_prompt","args":{"prompt":{"intro":{"kind":"host","text":"Operator recorded prompt"}}}},
            {"kind":"set_rlm_prompt_context","args":{"context":["Operator context"]}},
            {"kind":"set_rlm_render","args":{"print":{},"preview":{}}}
        ]);
        let applied = apply_config(&h, revision, "core-and-rlm", commands).await;
        assert_eq!(applied["outcome"]["kind"], "applied", "{applied}");
        let after = h.ok("GET", &h.session_path("/config"), None).await;
        assert_eq!(after["catalog"]["revision"], revision + 1);
        assert_eq!(after["recorded"]["policy"]["autonomous"], true);
        let stale = apply_config(
            &h,
            revision,
            "stale",
            json!([{"kind":"set_autonomy","args":{"autonomous":false}}]),
        )
        .await;
        assert_eq!(stale["outcome"]["kind"], "stale", "{stale}");
        assert_eq!(h.ok("GET", &h.session_path("/config"), None).await, after);
        let refused = apply_config(&h, revision + 1, "charge-refusal", json!([{"kind":"set_charge_safety","args":{"charge_safety":{"mode":"accept_duplicate_billing","max_unsafe_retries":255,"max_duplicate_cost_tokens":null}}}])).await;
        assert_eq!(refused["outcome"]["kind"], "refused", "{refused}");
    });
}
#[test]
fn recorded_standard_prompt_and_render_commands_use_the_standard_owner() {
    run_async_test_on_stack_budget("operator-standard-config", || async {
        let h = OperatorHarness::new(true, false).await;
        let revision = h.ok("GET", &h.session_path("/config"), None).await["catalog"]["revision"]
            .as_u64()
            .unwrap();
        let result = apply_config(&h, revision, "standard-prompt", json!([
            {"kind":"set_standard_prompt","args":{"prompt":{"intro":"Recorded standard prompt"}}},
            {"kind":"set_standard_prompt_context","args":{"context":["Recorded context"]}},
            {"kind":"set_standard_render","args":{"render":null}}
        ])).await;
        assert_eq!(result["outcome"]["kind"], "applied", "{result}");
        assert!(
            h.ok("GET", &h.session_path("/config"), None).await["recorded"]["plugins"]
                .to_string()
                .contains("Recorded context")
        );
    });
}
#[test]
fn command_submit_settle_withdraw_and_compact_preserve_receipts() {
    run_async_test_on_stack_budget("operator-commands", || async {
        let h = OperatorHarness::new(true, false).await;
        let hold = h
            .double
            .hold_session_shift(&h.state.current_session_id())
            .await;
        let withdrawn = h.ok("POST", &h.session_path("/commands"), Some(json!({"id":"withdraw-first","command":{"kind":"refresh_tool_catalog","reason":"withdrawn"}}))).await;
        assert_eq!(
            h.ok(
                "POST",
                &h.session_path("/commands/withdraw"),
                Some(withdrawn.clone())
            )
            .await["kind"],
            "withdrawn"
        );
        drop(hold);
        assert_eq!(
            h.ok("POST", &h.session_path("/commands/settle"), Some(withdrawn))
                .await["kind"],
            "cancelled"
        );
        let body = json!({"id":"operator-refresh","command":{"kind":"refresh_tool_catalog","reason":"operator"}});
        let receipt = h
            .ok("POST", &h.session_path("/commands"), Some(body.clone()))
            .await;
        assert_eq!(
            h.ok("POST", &h.session_path("/commands"), Some(body)).await,
            receipt
        );
        let result = h
            .ok(
                "POST",
                &h.session_path("/commands/settle"),
                Some(receipt.clone()),
            )
            .await;
        assert_eq!(result["kind"], "durable");
        let result = h
            .ok(
                "POST",
                &h.session_path("/commands/withdraw"),
                Some(receipt.clone()),
            )
            .await;
        assert_eq!(result["kind"], "already_admitted");
        let mut foreign = receipt;
        foreign["session_id"] = json!("another-session");
        assert_eq!(
            h.request("POST", &h.session_path("/commands/withdraw"), Some(foreign))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let compact = h
            .ok(
                "POST",
                &h.session_path("/compact"),
                Some(json!({"instructions":null})),
            )
            .await;
        assert_eq!(compact["opened"], false);
    });
}

#[test]
fn every_operator_route_checks_deployment_authorization_before_work() {
    run_async_test_on_stack_budget("operator-authorization", || async {
        struct DenyOperators;
        impl WorkbenchAuthorizer for DenyOperators {
            fn authorize(&self, action: &WorkbenchAuthorizationAction) -> Result<(), AppError> {
                assert!(matches!(
                    action,
                    WorkbenchAuthorizationAction::OperateDeployment
                ));
                Err(AppError::forbidden("operator access denied"))
            }
        }
        let h = OperatorHarness::new(false, false).await;
        let mut state = h.state.clone();
        state.authorization = WorkbenchAuthorization::with_authorizer(Arc::new(DenyOperators));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = operator_routes().with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let session_id = h.state.current_session_id();
        let receipt =
            json!({"session_id":session_id,"batch_id":"qwb:unknown","source_key":"command:test"});
        let park = json!({"target":{"kind":"turn","session_id":session_id,"turn_id":"missing"},"park_id":1});
        let requests = vec![
            ("GET", "/api/admin/parks".into(), None),
            ("GET", "/api/admin/parks/events".into(), None),
            (
                "POST",
                "/api/admin/parks/redrive".into(),
                Some(park.clone()),
            ),
            ("POST", "/api/admin/parks/cancel".into(), Some(park.clone())),
            ("POST", "/api/admin/parks/fork".into(), Some(park)),
            (
                "GET",
                "/api/admin/drain?accepting_new_work=true".into(),
                None,
            ),
            (
                "GET",
                "/api/admin/generations/aaaaaaaaaaaa/drain".into(),
                None,
            ),
            (
                "POST",
                "/api/admin/generations/aaaaaaaaaaaa/drain".into(),
                None,
            ),
            (
                "DELETE",
                "/api/admin/generations/aaaaaaaaaaaa/drain".into(),
                None,
            ),
            ("GET", "/api/admin/obligations/ingress".into(), None),
            (
                "POST",
                "/api/admin/obligations/rearm".into(),
                Some(json!({"kind":"ingress","id":"missing"})),
            ),
            ("GET", h.session_path("/usage/facts"), None),
            ("GET", h.session_path("/usage/meters"), None),
            ("POST", h.session_path("/usage/reconcile"), None),
            ("GET", h.session_path("/config"), None),
            (
                "POST",
                h.session_path("/config"),
                Some(
                    json!({"id":"denied","expected_revision":0,"commands":[{"kind":"set_autonomy","args":{"autonomous":true}}]}),
                ),
            ),
            (
                "POST",
                h.session_path("/config/settle"),
                Some(receipt.clone()),
            ),
            (
                "POST",
                h.session_path("/commands"),
                Some(
                    json!({"id":"denied","command":{"kind":"refresh_tool_catalog","reason":"denied"}}),
                ),
            ),
            (
                "POST",
                h.session_path("/commands/settle"),
                Some(receipt.clone()),
            ),
            ("POST", h.session_path("/commands/withdraw"), Some(receipt)),
            (
                "POST",
                h.session_path("/compact"),
                Some(json!({"instructions":null})),
            ),
        ];
        for (method, path, body) in requests {
            let mut request =
                reqwest::Client::new().request(method.parse().unwrap(), format!("{base}{path}"));
            if let Some(body) = body {
                request = request.json(&body);
            }
            let result = request.send().await.unwrap();
            assert_eq!(result.status(), StatusCode::FORBIDDEN, "{method} {path}");
        }
        server.abort();
        let _ = server.await;
        assert!(
            h.state
                .core
                .session(session_id)
                .durable()
                .await
                .unwrap()
                .queued_work()
                .await
                .unwrap()
                .is_empty()
        );
    });
}

#[test]
fn operator_panel_targets_the_selected_session_and_retains_pending_receipts() {
    use std::io::Write;

    let node = std::env::var_os("LASH_WORKBENCH_TEST_NODE").unwrap_or_else(|| "node".into());
    let script = r#"
const assert = require('node:assert/strict');
const vm = require('node:vm');
const html = require('node:fs').readFileSync(0, 'utf8');
const fullScript = html.split('<script>')[1].split('</script>')[0];
new vm.Script(fullScript);
const source = fullScript.slice(fullScript.indexOf('    const operatorDialog ='), fullScript.lastIndexOf('    renderShellStatus();'));
const nodes = new Map([...html.matchAll(/id="([^"]+)"/g)].map(match => [match[1], element(match[1])]));
function element(id) {
  return {id, value: '', disabled: false, tagName: id.endsWith('Form') ? 'FORM' : 'BUTTON', listeners: {}, children: [],
    addEventListener(event, action) { this.listeners[event] = action; },
    append(...children) { this.children.push(...children); if (!this.value && children[0]?.value) this.value = children[0].value; },
    replaceChildren() { this.children = []; }, showModal() {}, close() {}};
}
let sequence = 0;
const requests = [];
const receipt = {session_id: 'panel-session', batch_id: 'qwb:panel', source_key: 'command:panel'};
const context = {document: {
  getElementById(id) { assert.ok(nodes.has(id), 'missing panel node ' + id); return nodes.get(id); },
  createElement() { return element('created'); }
}, scopedSessionId: 'panel-session', modelInput: {value: 'test-model'},
crypto: {randomUUID: () => 'draft-' + ++sequence}, structuredClone, encodeURIComponent,
fetch: async (path, request) => {
  requests.push({path, ...request, payload: request.body ? JSON.parse(request.body) : undefined});
  const body = path.endsWith('/config') && request.method === 'GET' ? {catalog: {revision: 9}, recorded: {}}
    : path.endsWith('/config/settle') ? {kind: 'settled', outcome: {kind: 'stale', expected: 9, actual: 10}}
    : path.endsWith('/config') ? {kind: 'pending', receipt} : {records: [], next: null};
  return {ok: true, status: 200, text: async () => JSON.stringify(body)};
}};
vm.runInNewContext(source, context);
async function fire(id, event = 'click') {
  const currentTarget = nodes.get(id);
  await currentTarget.listeners[event]({currentTarget, submitter: element('submit'), preventDefault() {}});
}
(async () => {
  await fire('operatorOpen');
  await fire('operatorConfigRead');
  nodes.get('operatorConfigCommand').value = 'set_autonomy';
  nodes.get('operatorConfigArgs').value = '{"autonomous":true}';
  await fire('operatorConfigArgs', 'input');
  const id = nodes.get('operatorConfigId').value;
  await fire('operatorConfigForm', 'submit');
  const submission = requests.at(-1);
  assert.equal(submission.path, '/api/admin/sessions/panel-session/config');
  assert.equal(submission.payload.id, id);
  assert.equal(submission.payload.expected_revision, 9);
  assert.equal(submission.payload.commands[0].kind, 'set_autonomy');
  assert.equal(submission.payload.commands[0].args.autonomous, true);
  assert.equal(nodes.get('operatorConfigSettle').disabled, false);
  await fire('operatorConfigSettle');
  assert.deepEqual(requests.at(-1).payload, receipt);
  assert.match(nodes.get('operatorConfigResult').textContent, /stale/);
  await fire('operatorConfigForm', 'submit');
  assert.equal(requests.at(-1).payload.id, id, 'retry retains the same request id');
  await fire('operatorConfigArgs', 'input');
  assert.notEqual(nodes.get('operatorConfigId').value, id, 'an edited request gets a new identity');
})().catch(error => { console.error(error); process.exitCode = 1; });
"#;
    let mut child = std::process::Command::new(node)
        .arg("-e")
        .arg(script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(ui::INDEX_HTML.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
