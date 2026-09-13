use super::*;

#[derive(Clone, Copy, Debug)]
enum HostSetupFailureSite {
    DeferredResolution,
    HostEnvironment,
    ArtifactStore,
    RehydrateProjectedGlobals,
    ResolveProjectedBindings,
    CancelledSetup,
}

struct FailingArtifactStore;

#[async_trait::async_trait]
impl lashlang::LashlangArtifactStore for FailingArtifactStore {
    async fn put_module_artifact(
        &self,
        _artifact: &lashlang::ModuleArtifact,
    ) -> Result<(), lashlang::ArtifactStoreError> {
        Err(lashlang::ArtifactStoreError::Backend(
            "injected artifact store failure".to_string(),
        ))
    }

    async fn get_module_artifact(
        &self,
        _module_ref: &lashlang::ModuleRef,
    ) -> Result<Option<Arc<lashlang::ModuleArtifact>>, lashlang::ArtifactStoreError> {
        Ok(None)
    }

    async fn put_artifact_bytes(
        &self,
        _artifact_ref: &str,
        _descriptor: &str,
        _bytes: &[u8],
    ) -> Result<(), lashlang::ArtifactStoreError> {
        Ok(())
    }

    async fn get_artifact_bytes(
        &self,
        _artifact_ref: &str,
    ) -> Result<Option<Vec<u8>>, lashlang::ArtifactStoreError> {
        Ok(None)
    }
}

struct FailingProjectionResolver;

#[async_trait::async_trait]
impl ProjectionResolver for FailingProjectionResolver {
    async fn resolve_projection(
        &self,
        _reference: &ProjectionRef,
    ) -> Result<Arc<dyn ProjectedHostDescriptor>, crate::projection::ProjectionResolveError> {
        Err(crate::projection::ProjectionResolveError::invalid(
            "injected projection resolution failure",
        ))
    }
}

fn colliding_host_catalog() -> lash_core::ToolCatalog {
    let definition = |id, name| {
        lash_core::ToolDefinition::raw(
            id,
            name,
            "colliding test binding",
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "boolean" }),
        )
        .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(
            ["test"],
            "collision",
        ))
    };
    lash_core::ToolCatalog::from_tool_definitions(vec![
        definition("tool:collision_a", "collision_a"),
        definition("tool:collision_b", "collision_b"),
    ])
}

async fn inject_host_setup_failure(site: HostSetupFailureSite) -> ExecResponse {
    let mut state = RlmExecutionState::new();
    let mut context = lash_core::testing::code_execution_context();
    let mut request = ExecRequest {
        language: "lashlang".to_string(),
        code: "finish 1".to_string(),
    };
    let mut artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
        lashlang::global_in_memory_lashlang_artifact_store();
    let mut surface = LashlangSurface::default();
    let mut deferred_resolver = None;
    let mut projected_bindings = RlmProjectedBindings::default();
    let mut projection_resolver: Arc<dyn ProjectionResolver> = Arc::new(ProjectionRegistry::new());

    match site {
        HostSetupFailureSite::DeferredResolution => {
            let provider: Arc<dyn lash_core::ToolProvider> =
                Arc::new(BindingRecordingDeferredProvider {
                    executions: Default::default(),
                    observed_bindings: Default::default(),
                    enumerations: Default::default(),
                });
            context = lash_core::testing::code_execution_context_with_tool_provider_catalog_effect_controller_and_invocation(
                provider,
                lash_core::ToolCatalog::default(),
                Arc::new(FailingDeferredJournalController),
                lash_core::testing::exec_code_invocation(
                    "host-setup-failure",
                    "turn-1",
                    0,
                    0,
                    "exec-code",
                    "exec-code:host-setup-failure",
                ),
            );
            request.code =
                r#"finish await web.fetch({ url: "https://example.test" })?"#.to_string();
            deferred_resolver = Some(Arc::new(BindingDeferredResolver {
                calls: Default::default(),
            })
                as lash_lashlang_runtime::SharedDeferredToolResolver);
        }
        HostSetupFailureSite::HostEnvironment => {
            context = lash_core::testing::code_execution_context_with_tool_catalog(
                colliding_host_catalog(),
            );
        }
        HostSetupFailureSite::ArtifactStore => {
            request.code = "process worker() { finish null }".to_string();
            artifact_store = Arc::new(FailingArtifactStore);
            surface = LashlangSurface::new(
                lashlang::LashlangAbilities::default().with_processes(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            );
        }
        HostSetupFailureSite::RehydrateProjectedGlobals => {
            request.code = "finish len(restored)".to_string();
            let registry = Arc::new(ProjectionRegistry::new());
            let descriptor = Arc::new(SnapshotProjectedToolText::default());
            let reference = registry.register_memory(descriptor.clone());
            state
                .rlm
                .insert_global(
                    "restored",
                    FlowValue::List(
                        (0..256)
                            .map(|index| {
                                FlowValue::Projected(ProjectedValue::custom_with_projection_ref(
                                    format!("restored[{index}]"),
                                    descriptor.clone(),
                                    serde_json::json!(reference),
                                ))
                            })
                            .collect::<Vec<_>>()
                            .into(),
                    ),
                )
                .expect("insert projected values before heap activation");
            let first = execute_code_with_dialect_and_bounds(
                &mut state,
                context.clone(),
                request.clone(),
                artifact_store.clone(),
                surface.clone(),
                None,
                RlmProjectedBindings::default(),
                registry.clone(),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::new(
                    lashlang::ExecutionBound::Unbounded,
                    lashlang::ExecutionBound::Unbounded,
                    lashlang::ExecutionBound::logical_bytes(40 * 1024),
                ),
                RlmSourceContext::cell(SourceDialect::Lashlang),
            )
            .await;
            assert_eq!(first.error, None, "activate a bounded heap");
            projection_resolver = registry;
        }
        HostSetupFailureSite::ResolveProjectedBindings => {
            projected_bindings = RlmProjectedBindings::new()
                .bind_lazy(
                    "doc",
                    ProjectionRef::new("injected", serde_json::json!("missing")),
                )
                .expect("bind injected projection");
            projection_resolver = Arc::new(FailingProjectionResolver);
        }
        HostSetupFailureSite::CancelledSetup => {
            context = lash_core::testing::cancelled_code_execution_context();
            request.code = "missing =".to_string();
        }
    }

    execute_code_unbounded_for_tests(
        &mut state,
        context,
        request,
        artifact_store,
        surface,
        deferred_resolver,
        projected_bindings,
        projection_resolver,
        RlmLashlangExecutionTraceConfig::default(),
    )
    .await
}

#[test]
pub(super) fn every_host_setup_failure_is_classified_as_host() {
    block_on(async {
        // The seventh `Host` classification in `executor/mod.rs` is the
        // `LinkedProgramCacheError` catch-all. It intentionally has no row:
        // the cache currently constructs only `Parse` and `Link`, which are
        // classified by the preceding arms, so no host-classified variant can
        // be injected through its public API.
        let cases = [
            (
                HostSetupFailureSite::DeferredResolution,
                "injected deferred journal commit failure",
            ),
            (
                HostSetupFailureSite::HostEnvironment,
                "invalid Lashlang host tool surface",
            ),
            (
                HostSetupFailureSite::ArtifactStore,
                "injected artifact store failure",
            ),
            (
                HostSetupFailureSite::RehydrateProjectedGlobals,
                "logical memory limit",
            ),
            (
                HostSetupFailureSite::ResolveProjectedBindings,
                "injected projection resolution failure",
            ),
            (
                HostSetupFailureSite::CancelledSetup,
                "foreground execution stopped during setup",
            ),
        ];

        for (site, expected_message) in cases {
            let error = Box::pin(inject_host_setup_failure(site))
                .await
                .error
                .unwrap_or_else(|| panic!("{site:?}: injected setup failure must be observed"));
            assert!(
                error.message.contains(expected_message),
                "{site:?}: wrong setup path reached: {error:?}"
            );
            assert_eq!(
                error.kind,
                lash_core::CellFailureKind::Host,
                "{site:?}: host setup failures must not blame the program"
            );
        }
    });
}

#[test]
pub(super) fn host_cancellation_is_a_terminal_stop_not_a_program_error() {
    assert_eq!(
        lashlang_runtime_feedback_kind(&lashlang::RuntimeError::HostCancelled, false),
        lash_core::CellFailureKind::Host
    );
    assert_eq!(
        lashlang_runtime_feedback_kind(
            &lashlang::RuntimeError::SleepFailed {
                source: lashlang::ExecutionHostError::new("cancelled wait"),
            },
            true,
        ),
        lash_core::CellFailureKind::Host,
        "host cancellation wins when an active wait unwinds through a host error"
    );
}

#[test]
pub(super) fn execution_started_inventory_matches_lifecycle_for_both_dialects() {
    block_on(async {
        for (language, source, dialect) in [
            (
                "lashlang",
                "first = await web.fetch({ url: \"a\" })?\nsecond = await web.fetch({ url: \"b\" })?\nfinish second",
                SourceDialect::Lashlang,
            ),
            (
                "typescript",
                "const first = await web.fetch({ url: 'a' }); const second = await web.fetch({ url: 'b' }); finish(second);",
                SourceDialect::Typescript,
            ),
        ] {
            let evidence = execute_and_collect_inventory(source, language, dialect).await;
            assert!(
                evidence.declared.len() > 1,
                "{language}: regression program must exercise multiple nodes, got {:?}",
                evidence.declared
            );
            assert_eq!(
                evidence.lifecycle_event_count,
                evidence.lifecycle.len() * 2,
                "{language}: every deterministic node must emit started and completed lifecycle events"
            );

            let declared_ids = evidence.declared.keys().collect::<Vec<_>>();
            let lifecycle_ids = evidence.lifecycle.keys().collect::<Vec<_>>();
            assert_eq!(
                declared_ids, lifecycle_ids,
                "{language}: execution_started and lifecycle node-id sets must be equal"
            );

            for (node_id, (node_kind, node_label)) in &evidence.lifecycle {
                let (declared_kind, declared_label) = evidence
                    .declared
                    .get(node_id)
                    .expect("lifecycle node is pre-declared");
                assert_eq!(
                    (node_kind, node_label),
                    (declared_kind, declared_label),
                    "{language}: lifecycle metadata must match execution_started for {node_id}"
                );
            }
        }
    });
}

#[derive(Debug)]
pub(super) struct InventoryEvidence {
    declared: BTreeMap<String, (String, String)>,
    lifecycle: BTreeMap<String, (String, String)>,
    lifecycle_event_count: usize,
}

#[derive(Default)]
pub(super) struct RecordingTraceSink(Mutex<Vec<TraceLanguageExecution>>);

impl TraceSink for RecordingTraceSink {
    fn append(
        &self,
        record: &lash_core::facade_support::TraceRecord,
    ) -> Result<(), lash_core::facade_support::TraceSinkError> {
        if let lash_core::TraceEvent::LanguageExecution { event, .. } = &record.event {
            self.0.lock().expect("trace sink lock").push(event.clone());
        }
        Ok(())
    }
}

pub(super) async fn execute_and_collect_inventory(
    source: &str,
    language: &str,
    dialect: SourceDialect,
) -> InventoryEvidence {
    let sink = Arc::new(RecordingTraceSink::default());
    let context =
        lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
            Arc::new(BindingRecordingDeferredProvider {
                executions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                observed_bindings: Arc::new(std::sync::Mutex::new(Vec::new())),
                enumerations: Default::default(),
            }),
            lash_core::ToolCatalog::default(),
            lash_core::testing::exec_code_invocation(
                format!("fig2365-{language}"),
                "turn-1",
                1,
                1,
                format!("exec-{language}"),
                format!("exec:{language}"),
            ),
        );
    let mut state = RlmExecutionState::for_engine(language);
    let response = execute_code_with_dialect_and_bounds(
        &mut state,
        context,
        ExecRequest {
            language: language.to_string(),
            code: source.to_string(),
        },
        lashlang::global_in_memory_lashlang_artifact_store(),
        LashlangSurface::default(),
        Some(Arc::new(BindingDeferredResolver {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })),
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig {
            sink: Some(sink.clone()),
            trace_context: TraceContext::default(),
        },
        lashlang::ExecutionBounds::unbounded(),
        RlmSourceContext::cell(dialect),
    )
    .await;
    assert_eq!(
        response.error, None,
        "{language}: regression program executes"
    );

    let events = sink.0.lock().expect("trace sink lock").clone();
    let started = events
        .iter()
        .filter_map(|event| match &event.payload {
            TraceLanguageExecutionPayload::ExecutionStarted { execution_map } => {
                Some(execution_map)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(started.len(), 1, "{language}: one execution_started event");

    let mut declared = BTreeMap::new();
    for node in &started[0].nodes {
        assert!(
            declared
                .insert(node.id.clone(), (node.kind.clone(), node.label.clone()))
                .is_none(),
            "{language}: execution_started contains duplicate node id {}",
            node.id
        );
    }
    let mut lifecycle = BTreeMap::new();
    let mut lifecycle_event_count = 0;
    for event in events {
        let node = match event.payload {
            TraceLanguageExecutionPayload::NodeStarted {
                node_id,
                node_kind,
                label,
                ..
            }
            | TraceLanguageExecutionPayload::NodeCompleted {
                node_id,
                node_kind,
                label,
                ..
            }
            | TraceLanguageExecutionPayload::NodeFailed {
                node_id,
                node_kind,
                label,
                ..
            } => Some((node_id, node_kind, label)),
            _ => None,
        };
        if let Some((node_id, node_kind, label)) = node {
            lifecycle_event_count += 1;
            if let Some(previous) = lifecycle.insert(node_id.clone(), (node_kind, label)) {
                assert_eq!(
                    lifecycle.get(&node_id),
                    Some(&previous),
                    "{language}: lifecycle metadata changed for {node_id}"
                );
            }
        }
    }

    InventoryEvidence {
        declared,
        lifecycle,
        lifecycle_event_count,
    }
}

#[test]
pub(super) fn cancelled_execution_reaches_the_stop_classifier_in_both_dialects() {
    block_on(async {
        for (language, successful_code, code, cancelled_binding, source_dialect) in [
            (
                "lashlang",
                "survives = 7",
                "cancelled_tail = 1\nwhile true {}",
                "cancelled_tail",
                SourceDialect::Lashlang,
            ),
            (
                "typescript",
                "let survives: number = 7;",
                "let cancelledTail: number = 1; while (true) {}",
                "cancelledTail",
                SourceDialect::Typescript,
            ),
        ] {
            let mut state = RlmExecutionState::for_engine(language);
            let successful = execute_code_with_dialect_and_bounds(
                &mut state,
                lash_core::testing::code_execution_context(),
                ExecRequest {
                    language: language.to_string(),
                    code: successful_code.to_string(),
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                RlmSourceContext::cell(source_dialect),
            )
            .await;
            assert_eq!(successful.error, None, "{language}: first cell");

            let response = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                execute_code_with_dialect_and_bounds(
                    &mut state,
                    lash_core::testing::code_execution_context_cancelling_after_yield(),
                    ExecRequest {
                        language: language.to_string(),
                        code: code.to_string(),
                    },
                    lashlang::global_in_memory_lashlang_artifact_store(),
                    LashlangSurface::default(),
                    None,
                    RlmProjectedBindings::default(),
                    Arc::new(ProjectionRegistry::new()),
                    RlmLashlangExecutionTraceConfig::default(),
                    lashlang::ExecutionBounds::unbounded(),
                    RlmSourceContext::cell(source_dialect),
                ),
            )
            .await
            .unwrap_or_else(|_| panic!("{language}: running code did not observe cancellation"));

            let error = response.error.expect("host cancellation is classified");
            assert_eq!(
                error.kind,
                lash_core::CellFailureKind::Host,
                "{language}: cancellation must never be classified as a program failure"
            );
            assert!(
                state.rlm.globals().get(cancelled_binding).is_none(),
                "{language}: the cancelled tail must roll back to the execution checkpoint"
            );
            assert!(
                state.rlm.globals().get("survives").is_some(),
                "{language}: the earlier successful cell must remain live"
            );

            let snapshot = hydrate_snapshot(
                state
                    .snapshot_execution_state()
                    .expect("snapshot after cancelled cell"),
            );
            let mut restored = RlmExecutionState::for_engine(language);
            restored
                .restore_execution_state(&snapshot)
                .expect("cold restore after cancelled cell");
            assert!(
                restored.rlm.globals().get("survives").is_some(),
                "{language}: a cold restore must retain the earlier successful cell"
            );
            assert!(
                restored.rlm.globals().get(cancelled_binding).is_none(),
                "{language}: a cold restore must exclude the cancelled tail"
            );
        }
    });
}

#[test]
pub(super) fn cancellation_wins_over_pre_execution_compile_failures() {
    block_on(async {
        for (language, code, source_dialect) in [
            ("lashlang", "missing =", SourceDialect::Lashlang),
            (
                "typescript",
                "let missing: number = ;",
                SourceDialect::Typescript,
            ),
        ] {
            let mut state = RlmExecutionState::for_engine(language);
            let response = execute_code_with_dialect_and_bounds(
                &mut state,
                lash_core::testing::cancelled_code_execution_context(),
                ExecRequest {
                    language: language.to_string(),
                    code: code.to_string(),
                },
                lashlang::global_in_memory_lashlang_artifact_store(),
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                Arc::new(ProjectionRegistry::new()),
                RlmLashlangExecutionTraceConfig::default(),
                lashlang::ExecutionBounds::unbounded(),
                RlmSourceContext::cell(source_dialect),
            )
            .await;

            let error = response.error.expect("cancelled setup is classified");
            assert_eq!(
                error.kind,
                lash_core::CellFailureKind::Host,
                "{language}: observed cancellation must win over a compile failure"
            );
        }
    });
}

#[test]
pub(super) fn late_cancellation_settlement_rolls_back_only_the_uncommitted_cell() {
    block_on(async {
        for (language, first_code, tail_code, tail_binding, source_dialect) in [
            (
                "lashlang",
                "survives = 7",
                "cancelled_tail = 1",
                "cancelled_tail",
                SourceDialect::Lashlang,
            ),
            (
                "typescript",
                "let survives: number = 7;",
                "let cancelledTail: number = 1;",
                "cancelledTail",
                SourceDialect::Typescript,
            ),
        ] {
            let mut state = RlmExecutionState::for_engine(language);
            for code in [first_code, tail_code] {
                let response = execute_code_with_dialect_and_bounds(
                    &mut state,
                    lash_core::testing::code_execution_context(),
                    ExecRequest {
                        language: language.to_string(),
                        code: code.to_string(),
                    },
                    lashlang::global_in_memory_lashlang_artifact_store(),
                    LashlangSurface::default(),
                    None,
                    RlmProjectedBindings::default(),
                    Arc::new(ProjectionRegistry::new()),
                    RlmLashlangExecutionTraceConfig::default(),
                    lashlang::ExecutionBounds::unbounded(),
                    RlmSourceContext::cell(source_dialect),
                )
                .await;
                assert_eq!(response.error, None, "{language}: `{code}`");
            }

            assert!(state.rlm.globals().get(tail_binding).is_some());
            state.cancel_code_execution();
            assert!(state.rlm.globals().get(tail_binding).is_none());
            assert!(state.rlm.globals().get("survives").is_some());

            let snapshot = hydrate_snapshot(
                state
                    .snapshot_execution_state()
                    .expect("snapshot after late cancellation"),
            );
            let mut restored = RlmExecutionState::for_engine(language);
            restored
                .restore_execution_state(&snapshot)
                .expect("cold restore after late cancellation");
            assert!(restored.rlm.globals().get(tail_binding).is_none());
            assert!(restored.rlm.globals().get("survives").is_some());
        }
    });
}

#[test]
pub(super) fn late_cancellation_preserves_staged_and_acknowledged_large_leaf_bookkeeping() {
    block_on(async {
        for (language, first_code, tail_code, tail_binding, source_dialect) in [
            (
                "lashlang",
                format!("survives = \"{}\"", "x".repeat(1024)),
                "cancelled_tail = 1",
                "cancelled_tail",
                SourceDialect::Lashlang,
            ),
            (
                "typescript",
                format!("let survives: string = \"{}\";", "x".repeat(1024)),
                "let cancelledTail: number = 1;",
                "cancelledTail",
                SourceDialect::Typescript,
            ),
        ] {
            for acknowledge_first_capture in [false, true] {
                let mut state = RlmExecutionState::for_engine(language);
                let first = execute_code_with_dialect_and_bounds(
                    &mut state,
                    lash_core::testing::code_execution_context(),
                    ExecRequest {
                        language: language.to_string(),
                        code: first_code.clone(),
                    },
                    lashlang::global_in_memory_lashlang_artifact_store(),
                    LashlangSurface::default(),
                    None,
                    RlmProjectedBindings::default(),
                    Arc::new(ProjectionRegistry::new()),
                    RlmLashlangExecutionTraceConfig::default(),
                    lashlang::ExecutionBounds::unbounded(),
                    RlmSourceContext::cell(source_dialect),
                )
                .await;
                assert_eq!(first.error, None, "{language}: large first cell");
                let first_snapshot = state
                    .snapshot_execution_state()
                    .expect("large first-cell snapshot");
                let first_hydration = hydrate_snapshot(first_snapshot);
                if acknowledge_first_capture {
                    state.acknowledge_execution_state_capture();
                }

                let tail = execute_code_with_dialect_and_bounds(
                    &mut state,
                    lash_core::testing::code_execution_context(),
                    ExecRequest {
                        language: language.to_string(),
                        code: tail_code.to_string(),
                    },
                    lashlang::global_in_memory_lashlang_artifact_store(),
                    LashlangSurface::default(),
                    None,
                    RlmProjectedBindings::default(),
                    Arc::new(ProjectionRegistry::new()),
                    RlmLashlangExecutionTraceConfig::default(),
                    lashlang::ExecutionBounds::unbounded(),
                    RlmSourceContext::cell(source_dialect),
                )
                .await;
                assert_eq!(tail.error, None, "{language}: tail cell");
                state.cancel_code_execution();

                let final_snapshot = state
                    .snapshot_execution_state()
                    .expect("snapshot after late cancellation");
                let final_hydration = if acknowledge_first_capture {
                    hydrate_snapshot_against(final_snapshot, &first_hydration)
                } else {
                    hydrate_snapshot(final_snapshot)
                };
                let mut restored = RlmExecutionState::for_engine(language);
                restored
                    .restore_execution_state(&final_hydration)
                    .expect("cold restore after late cancellation");
                assert!(restored.rlm.globals().get("survives").is_some());
                assert!(restored.rlm.globals().get(tail_binding).is_none());
            }
        }
    });
}

#[test]
pub(super) fn parse_diagnostic_warns_about_multiline_cell_delimiters() {
    let code = "payload = \"\"\"";
    let error = lashlang::parse(code).expect_err("unterminated multiline string");
    let diagnostic = format_rlm_parse_diagnostic(code, &error, crate::plugin::RlmChannel::Cell);
    assert!(diagnostic.contains("standalone `</lashlang>` line"));
    assert!(diagnostic.contains("inside multiline source text"));
}

/// The native `execute_code` channel (ADR 0083) has no cell tags, so the
/// delimiter sentence would send the model hunting for syntax it never
/// wrote. It gets the positioned diagnostic alone — the same diagnostic the
/// cell channel is given, without the cell-only advice appended.
#[test]
pub(super) fn native_channel_parse_diagnostic_omits_the_cell_delimiter_hint() {
    let code = "payload = \"\"\"";
    let error = lashlang::parse(code).expect_err("unterminated multiline string");
    let positioned = lashlang::format_parse_diagnostic(code, &error);

    let native = format_rlm_parse_diagnostic(code, &error, crate::plugin::RlmChannel::NativeTool);
    assert_eq!(native, positioned);
    assert!(!native.contains("</lashlang>"), "{native}");
    assert!(!native.contains("standalone delimiter line"), "{native}");

    let cell = format_rlm_parse_diagnostic(code, &error, crate::plugin::RlmChannel::Cell);
    assert_eq!(
        cell.strip_prefix(positioned.as_str())
            .expect("the cell diagnostic is the same diagnostic plus the hint")
            .trim(),
        "A standalone `</lashlang>` line terminates the outer cell even inside multiline source text; construct that content without a standalone delimiter line."
    );
}

/// The channel reaches the formatter from the session's pinned config, not
/// from a default at the executor's door: an `RlmSourceContext` carries the
/// dialect and the channel together, so a new execution path cannot reach
/// the formatter without saying which channel it is.
#[test]
pub(super) fn source_context_carries_the_channel_alongside_the_dialect() {
    let cell = RlmSourceContext::cell(SourceDialect::Typescript);
    assert_eq!(cell.dialect, SourceDialect::Typescript);
    assert_eq!(cell.channel, crate::plugin::RlmChannel::Cell);

    let native = RlmSourceContext::new(
        SourceDialect::Lashlang,
        crate::plugin::RlmChannel::NativeTool,
    );
    assert_eq!(native.dialect, SourceDialect::Lashlang);
    assert_eq!(native.channel, crate::plugin::RlmChannel::NativeTool);
}

/// A typo is not a policy refusal.
///
/// Classifying every compile failure as Policy produced the one thing
/// the typed distinction exists to prevent: `unknown name \`task\`` arrived under "the
/// runtime refused this cell; sending it again unchanged will be refused
/// again. Rewrite it in the form named above" — with no form named above,
/// because a misspelled identifier has no accepted alternative form. The
/// gate is the diagnostic code, not the fact that compilation failed.
#[test]
pub(super) fn a_wrong_program_and_a_forbidden_construct_are_classified_apart() {
    let typo = lash_typescript::parse_with_globals("finish(taks);", &BTreeSet::new())
        .expect_err("an unbound name is rejected");
    assert_eq!(
        typescript_feedback_kind(&typo),
        lash_core::CellFailureKind::Program,
        "a misspelled name is the program being wrong: {typo}"
    );

    let forbidden = lash_typescript::parse_with_globals("class A {}", &BTreeSet::new())
        .expect_err("classes are refused");
    assert_eq!(
        typescript_feedback_kind(&forbidden),
        lash_core::CellFailureKind::Policy,
        "a construct outside the dialect is a refusal: {forbidden}"
    );

    // And the imperative the Policy branch chooses is only honest when the
    // diagnostic really does name a form.
    assert!(
        !forbidden.suggestions.is_empty(),
        "a Policy classification promises a named form: {forbidden:?}"
    );

    // One code, both families. `TS_METHOD_UNSUPPORTED` is emitted both for
    // the determinism refusals — which the runtime will never run, however
    // the model rewrites them — and for ordinary arity mistakes. Reading
    // the code alone gets one of the two wrong whichever way it is read.
    let nondeterministic =
        lash_typescript::parse_with_globals("finish('a'.localeCompare('b'));", &BTreeSet::new())
            .expect_err("locale ordering is refused");
    let miscounted = lash_typescript::parse_with_globals("finish([1].map());", &BTreeSet::new())
        .expect_err("map needs a callback");
    assert_eq!(
        nondeterministic.code.as_str(),
        miscounted.code.as_str(),
        "the premise of this check is that one code carries both"
    );
    assert_eq!(
        typescript_feedback_kind(&nondeterministic),
        lash_core::CellFailureKind::Policy,
        "the runtime will never run this: {nondeterministic}"
    );
    assert_eq!(
        typescript_feedback_kind(&miscounted),
        lash_core::CellFailureKind::Program,
        "the method exists and the call is wrong: {miscounted}"
    );
}

/// The executor is where a TypeScript rejection becomes the text a model
/// reads, and for the whole of the dialect's life that conversion was
/// `error.to_string()` — which drops the span the diagnostic carries. The
/// model was told a construct was refused and left to find it.
#[test]
pub(super) fn a_typescript_rejection_reaches_the_model_with_its_own_line_number() {
    let code = "const rows = [1, 2, 3];\nconst total = 0;\nclass Accumulator {}\n";
    let error = lash_typescript::parse_with_globals(code, &BTreeSet::new())
        .expect_err("classes are refused");
    let diagnostic = lash_typescript::format_diagnostic(code, &error);

    assert!(
        diagnostic.starts_with("TS_CLASS_UNSUPPORTED: "),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("--> line 3, column 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("\nclass Accumulator {}\n"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("\nhint: "), "{diagnostic}");
}

#[test]
pub(super) fn typescript_method_diagnostics_consult_the_link_time_module_catalog() {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation(
            ["text"],
            "TextModule",
            "sha256",
            "tool:text/sha256",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("text module operation");
    let environment =
        lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::default())
            .with_globals(["text"]);

    let shadowed_source = "text.sha256({});";
    let shadowed = lash_typescript::parse_with_globals_and_process_handles(
        shadowed_source,
        &environment.globals,
        &environment.process_handles,
    )
    .expect_err("the cache parse does not carry the module catalog");
    let shadowed = refine_typescript_method_diagnostic(shadowed_source, &environment, shadowed);
    assert_eq!(
        shadowed.message,
        "local binding `text` shadows module `text`; rename the binding or call the module before binding"
    );

    let ordinary_source = "const s = 'a,b'; s.notAMethod(',');";
    let ordinary = lash_typescript::parse_with_globals_and_process_handles(
        ordinary_source,
        &environment.globals,
        &environment.process_handles,
    )
    .expect_err("an ordinary local method remains unsupported");
    let ordinary = refine_typescript_method_diagnostic(ordinary_source, &environment, ordinary);
    assert_eq!(
        ordinary.message,
        "method `notAMethod` is not in the TypeScript runtime surface"
    );
}
static EXECUTION_BOUND_EXHAUSTION_MODE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Default)]
pub(super) struct NoopHost;

impl ExecutionHost for NoopHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => Err(ExecutionHostError::new(format!(
                "unknown module operation: {}",
                operation.operation
            ))),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

pub(super) async fn execute_with_projected(
    compiled: &lashlang::CompiledProgram,
    state: &mut lashlang::State,
    projected: &ProjectedBindings,
) -> Result<ExecutionOutcome, lashlang::RuntimeError> {
    let env = ExecutionEnvironment::new(&NoopHost).with_projected_bindings(projected.clone());
    lashlang::execute(compiled, state, &env).await
}

pub(super) fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

pub(super) fn hydrate_snapshot(
    snapshot: lash_core::plugin::ExecutionStateSnapshot,
) -> lash_core::plugin::HydratedExecutionState {
    lash_core::plugin::HydratedExecutionState {
        root: snapshot.root.expect("snapshot root"),
        components: snapshot
            .components
            .into_iter()
            .map(|(key, component)| match component {
                lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) => (key, body),
                lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged => {
                    panic!("fresh test snapshot unexpectedly reused `{key}`")
                }
            })
            .collect(),
    }
}

pub(super) fn hydrate_snapshot_against(
    snapshot: lash_core::plugin::ExecutionStateSnapshot,
    prior: &lash_core::plugin::HydratedExecutionState,
) -> lash_core::plugin::HydratedExecutionState {
    lash_core::plugin::HydratedExecutionState {
        root: snapshot.root.expect("snapshot root"),
        components: snapshot
            .components
            .into_iter()
            .map(|(key, component)| match component {
                lash_core::plugin::ExecutionStateComponentSnapshot::Changed(body) => (key, body),
                lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged => {
                    let body = prior
                        .components
                        .get(&key)
                        .unwrap_or_else(|| panic!("durable prior is missing leaf `{key}`"))
                        .clone();
                    (key, body)
                }
            })
            .collect(),
    }
}

#[derive(Default)]
pub(super) struct NoopTraceSink;

impl lash_core::facade_support::TraceSink for NoopTraceSink {
    fn append(
        &self,
        _record: &lash_core::facade_support::TraceRecord,
    ) -> Result<(), lash_core::facade_support::TraceSinkError> {
        Ok(())
    }
}

pub(super) async fn execute_continue_as_with_trace_sink(
    trace_sink: Option<Arc<dyn lash_core::facade_support::TraceSink>>,
) -> lash_core::ToolCallRecord {
    let definition = crate::continue_as_tool_definition();
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![definition]);
    let invocation = lash_core::testing::exec_code_invocation(
        "test-session",
        "turn-7",
        7,
        2,
        "exec-code-3",
        "exec-code:3",
    );
    let context =
        lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
            Arc::new(crate::control_tools::RlmControlToolsProvider {
                vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
            }),
            catalog,
            invocation,
        );
    let response = execute_code_unbounded_for_tests(
        &mut RlmExecutionState::new(),
        context,
        ExecRequest {
            language: "lashlang".to_string(),
            code: r#"await control.continue_as({ task: "continue deterministically" })?"#
                .to_string(),
        },
        lashlang::global_in_memory_lashlang_artifact_store(),
        LashlangSurface::default(),
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig {
            sink: trace_sink,
            trace_context: TraceContext::default(),
        },
    )
    .await;
    assert_eq!(response.error, None);
    assert_eq!(response.calls.len(), 1);
    response
        .calls
        .into_iter()
        .next()
        .and_then(|call| call.host_record)
        .expect("one continue_as host record")
}

#[test]
pub(super) fn resource_call_identity_is_trace_sink_independent() {
    block_on(async {
        let without_trace = Box::pin(execute_continue_as_with_trace_sink(None)).await;
        let with_trace = Box::pin(execute_continue_as_with_trace_sink(Some(Arc::new(
            NoopTraceSink,
        ))))
        .await;

        // Semantic hash v8 deliberately rekeys the module-rooted execution
        // site and the frame key derived from its call ID. Keep both literal
        // while proving trace configuration is absent from their inputs.
        assert_eq!(
            without_trace.call_id.as_deref(),
            Some(
                "lashlang:effect:{\"version\":2,\"kind\":\"turn\",\"session_id\":\"test-session\",\"execution_id\":\"turn-7\"}:\"exec-code:3\":resource:tool:continue_as:resource_operation:01082ba3b70f91a21c1b533f:1"
            )
        );
        assert_eq!(
            with_trace.call_id.as_deref(),
            Some(
                "lashlang:effect:{\"version\":2,\"kind\":\"turn\",\"session_id\":\"test-session\",\"execution_id\":\"turn-7\"}:\"exec-code:3\":resource:tool:continue_as:resource_operation:01082ba3b70f91a21c1b533f:1"
            )
        );

        let without_trace_key = match without_trace.output.control {
            Some(lash_core::ToolControl::SwitchAgentFrame { frame_key, .. }) => frame_key,
            other => panic!("expected frame switch, got {other:?}"),
        };
        let with_trace_key = match with_trace.output.control {
            Some(lash_core::ToolControl::SwitchAgentFrame { frame_key, .. }) => frame_key,
            other => panic!("expected frame switch, got {other:?}"),
        };
        assert_eq!(
            without_trace_key.as_str(),
            "frame-key/v2/b1a9a3d625981644571d577dffb3e6f525f9965fe8e639f9334547cd54dc2750"
        );
        assert_eq!(
            with_trace_key.as_str(),
            "frame-key/v2/b1a9a3d625981644571d577dffb3e6f525f9965fe8e639f9334547cd54dc2750"
        );
    });
}

pub(super) async fn execute_test_code(
    mut state: RlmExecutionState,
    code: String,
) -> RlmExecutionState {
    let response = Box::pin(execute_code_unbounded_for_tests(
        &mut state,
        lash_core::testing::code_execution_context(),
        ExecRequest {
            language: "lashlang".to_string(),
            code,
        },
        lashlang::global_in_memory_lashlang_artifact_store(),
        LashlangSurface::default(),
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
    ))
    .await;
    assert_eq!(response.error, None, "test Lashlang execution failed");
    state
}

pub(super) struct TestProjectedValue(Vec<FlowValue>);

#[derive(Default)]
pub(super) struct SnapshotProjectedToolText {
    pub(super) materialize_count: AtomicUsize,
    pub(super) render_count: AtomicUsize,
}

impl ProjectedHostDescriptor for SnapshotProjectedToolText {
    fn type_name(&self) -> &str {
        "string"
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, ProjectedReadResponse> {
        Box::pin(async move {
            match request {
                ProjectedReadRequest::Render => {
                    self.render_count.fetch_add(1, Ordering::SeqCst);
                    ProjectedReadResponse::Text("rendered tool text".to_string())
                }
                ProjectedReadRequest::Materialize => {
                    self.materialize_count.fetch_add(1, Ordering::SeqCst);
                    ProjectedReadResponse::Value(FlowValue::String("materialized tool text".into()))
                }
                _ => ProjectedReadResponse::Missing,
            }
        })
    }
}

impl ProjectedHostDescriptor for TestProjectedValue {
    fn type_name(&self) -> &str {
        "list"
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, ProjectedReadResponse> {
        Box::pin(async move {
            let ProjectedReadRequest::Index(index) = request else {
                return match request {
                    ProjectedReadRequest::Len => ProjectedReadResponse::Len(self.0.len()),
                    ProjectedReadRequest::Materialize => {
                        ProjectedReadResponse::Value(FlowValue::List(self.0.clone().into()))
                    }
                    _ => ProjectedReadResponse::Missing,
                };
            };
            let Ok(Some(index)) = projected_index(&index, self.0.len()) else {
                return ProjectedReadResponse::Missing;
            };
            self.0
                .get(index)
                .cloned()
                .map(ProjectedReadResponse::Value)
                .unwrap_or(ProjectedReadResponse::Missing)
        })
    }
}

pub(super) fn projected_history(values: Vec<FlowValue>) -> ProjectedBindings {
    let mut projected = ProjectedBindings::new();
    projected.insert(
        "history",
        ProjectedValue::custom("history", Arc::new(TestProjectedValue(values))),
    );
    projected
}

pub(super) async fn execute_with_lashlang_abilities(
    code: &str,
    abilities: lashlang::LashlangAbilities,
) -> ExecResponse {
    execute_with_lashlang_host_environment(code, abilities, lashlang::LashlangHostCatalog::new())
        .await
}

pub(super) async fn execute_with_lashlang_host_environment(
    code: &str,
    abilities: lashlang::LashlangAbilities,
    resources: lashlang::LashlangHostCatalog,
) -> ExecResponse {
    let mut state = RlmExecutionState::new();
    let ctx = if abilities.triggers {
        lash_core::testing::code_execution_context_with_trigger_store(Arc::new(
            lash_core::facade_support::InMemoryTriggerStore::default(),
        ))
    } else {
        lash_core::testing::code_execution_context()
    };
    let surface = LashlangSurface::new(
        abilities,
        lashlang::LashlangLanguageFeatures::default(),
        resources,
    );
    execute_code_with_bounds(
        &mut state,
        ctx,
        ExecRequest {
            language: "lashlang".to_string(),
            code: code.to_string(),
        },
        Arc::new(lashlang::InMemoryLashlangArtifactStore::new()),
        surface,
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::new(
            lashlang::ExecutionBound::instructions(1_000_000),
            lashlang::ExecutionBound::secs(30),
            lashlang::ExecutionBound::Unbounded,
        ),
    )
    .await
}

#[test]
#[should_panic(expected = "confidence execution exhausted a required Lashlang bound")]
pub(super) fn confidence_execution_fails_loudly_on_bound_exhaustion() {
    let _mode = EXECUTION_BOUND_EXHAUSTION_MODE.lock_recover();
    block_on(async {
        let _ = execute_code_with_bounds(
            &mut RlmExecutionState::new(),
            lash_core::testing::code_execution_context(),
            ExecRequest {
                language: "lashlang".to_string(),
                code: "i = 0\nwhile i < 5000 { i = i + 1 }\nfinish i".to_string(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::new(
                lashlang::ExecutionBound::instructions(1),
                lashlang::ExecutionBound::Unbounded,
                lashlang::ExecutionBound::Unbounded,
            ),
        )
        .await;
    });
}

#[test]
pub(super) fn exhaustion_response_remains_testable_when_loudness_is_temporarily_disabled() {
    let _mode = EXECUTION_BOUND_EXHAUSTION_MODE.lock_recover();
    block_on(async {
        let previous = set_execution_bound_exhaustion_loud(false);
        let result = execute_code_with_bounds(
            &mut RlmExecutionState::new(),
            lash_core::testing::code_execution_context(),
            ExecRequest {
                language: "lashlang".to_string(),
                code: "value = 1".to_string(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
            lashlang::ExecutionBounds::new(
                lashlang::ExecutionBound::instructions(1),
                lashlang::ExecutionBound::Unbounded,
                lashlang::ExecutionBound::Unbounded,
            ),
        )
        .await;
        set_execution_bound_exhaustion_loud(previous);
        assert!(
            result
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains("instruction budget"))
        );
    });
}

#[test]
pub(super) fn execute_code_reuses_linked_program_cache_for_repeat_source() {
    block_on(async {
        let mut state = RlmExecutionState::new();
        let request = || ExecRequest {
            language: "lashlang".to_string(),
            code: "finish 1".to_string(),
        };
        let resolver = || Arc::new(ProjectionRegistry::new());
        let surface = || {
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            )
        };

        let first = execute_code_unbounded_for_tests(
            &mut state,
            lash_core::testing::code_execution_context(),
            request(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            surface(),
            None,
            RlmProjectedBindings::default(),
            resolver(),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(first.terminal_finish, Some(serde_json::json!(1)));
        let first_stats = state.linked_programs.stats();
        assert_eq!(first_stats.hits, 0);
        assert_eq!(first_stats.misses, 1);

        let second = execute_code_unbounded_for_tests(
            &mut state,
            lash_core::testing::code_execution_context(),
            request(),
            lashlang::global_in_memory_lashlang_artifact_store(),
            surface(),
            None,
            RlmProjectedBindings::default(),
            resolver(),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert!(second.error.is_none(), "{:?}", second.error);
        assert_eq!(second.terminal_finish, Some(serde_json::json!(1)));
        let second_stats = state.linked_programs.stats();
        assert_eq!(second_stats.hits, 1);
        assert_eq!(second_stats.misses, 1);
        assert_eq!(second_stats.entries, 1);
        assert!(state.stored_lashlang_modules.is_empty());
    });
}
