use super::*;

use lash_sansio::ProcessId;

use lash_sansio::SessionId;

pub(super) const SESSION: &str = "intent-ingress-session";
pub(super) const SCOPE: &str = "intent-ingress-turn";

/// The id of the process a start intent registered, read off the handle its
/// executed outcome answers.
pub(super) fn started_process_id(outcome: &crate::tools::ToolIntentIngressOutcome) -> ProcessId {
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            lash_core::ToolIntentExecutionOutcome::Executed {
                realized: lash_core::ToolIntentRealized::StartProcess(result),
                ..
            },
        ..
    } = outcome
    else {
        panic!("the start executed: {outcome:?}");
    };
    result.process_id.clone()
}

/// Every process the registry holds.
async fn registered_process_count(registry: &Arc<dyn ProcessRegistry>) -> Result<usize> {
    Ok(lash_core::testing::process_roster_records_for_fixture(
        registry.as_ref(),
        &lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        },
    )
    .await?
    .len())
}

/// A held process start under the session's captured environment.
fn start_intent(session_id: &SessionId) -> lash_core::ToolIntent {
    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        owner: crate::RuntimeOwner::Session(session_id.clone()),
        declaration: lash_core::ProcessStartDeclaration::new(
            lash_core::testing::held_engine_input(serde_json::Value::Null),
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached,
        )
        .with_env_ref(session_env_ref()),
    }))
}

/// The environment an ingress test session captures.
fn session_env_ref() -> lash_core::ProcessExecutionEnvRef {
    (lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy {
            model: Some(recorded_llm_profile(mock_llm_profile_spec())),
            ..lash_core::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
                crate::NoProgressBudget::bounded(12),
            )
        },
        lash_core::SessionToolAccess::ambient(),
    ))
    .stable_ref()
    .expect("captured environment digest")
}

#[test]
fn ingress_start_without_lifetime_is_refused_before_submission() {
    let mut payload =
        serde_json::to_value(start_intent(&SessionId::from(SESSION))).expect("encode intent");
    fn remove_lifetime(value: &mut serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(object) => {
                if object.remove("lifetime").is_some() {
                    return true;
                }
                object.values_mut().any(remove_lifetime)
            }
            _ => false,
        }
    }
    assert!(
        remove_lifetime(&mut payload),
        "valid start had a required lifetime"
    );
    let error = serde_json::from_value::<lash_core::ToolIntent>(payload)
        .expect_err("missing lifetime must not decode");
    assert!(error.to_string().contains("lifetime"));
}

pub(super) const INGRESS_ENGINE_KIND: &str = "ingress-admission-engine";

/// Engine registered on the ingress host, so a submitted start can be checked
/// against a kind that exists and one that does not.
struct IngressAdmissionEngine;

#[async_trait::async_trait]
impl lash_core::ProcessEngine for IngressAdmissionEngine {
    async fn check_args(
        &self,
        _signature: &lash_core::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_core::ArgsMode,
    ) -> std::result::Result<(), lash_core::ArgsMismatch> {
        Err(lash_core::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

    fn kind(&self) -> &'static str {
        INGRESS_ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> std::result::Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _artifact_ref: &str,
    ) -> std::result::Result<(), lash_core::PluginError> {
        unreachable!("the ingress engine stores no artifacts")
    }

    fn state_format(&self) -> lash_core::EngineStateFormat {
        lash_core::EngineStateFormat {
            kind: self.kind().to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &lash_core::ProcessExecutionEnvSpec,
    ) -> std::result::Result<Option<serde_json::Value>, lash_core::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash_core::EngineState,
        event: lash_core::EngineEvent,
    ) -> std::result::Result<
        (lash_core::EngineState, lash_core::EngineAction),
        lash_core::ProcessInfraError,
    > {
        // The `refuse` program's first transition is refused: its process parks.
        if let lash_core::EngineEvent::Started { payload } = &event
            && payload.get("program").and_then(serde_json::Value::as_str) == Some("refuse")
        {
            return Err(lash_core::ProcessInfraError::new(
                lash_core::PluginError::Session("the ingress engine refuses this program".into()),
            ));
        }
        Ok((
            state,
            lash_core::EngineAction::Terminal(lash_core::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(serde_json::json!({"ingress_engine": "ran"})),
            )),
        ))
    }

    async fn resolve(
        &self,
        _reference: &lash_core::ProcessDefinitionRef,
    ) -> std::result::Result<
        lash_core::ProcessDefinitionResolution,
        lash_core::ProcessDefinitionRefusal,
    > {
        Ok(lash_core::ProcessDefinitionResolution::new(
            lash_core::ProcessSignature::Unknown,
        ))
    }
}

fn admit_ingress_engine(
    _kind: &'static str,
    payload: &serde_json::Value,
    env: Option<&lash_core::ProcessExecutionEnvSpec>,
) -> std::result::Result<lash_core::ProcessIdentity, lash_core::PluginError> {
    if payload.get("program").and_then(serde_json::Value::as_str) == Some("invalid") {
        return Err(lash_core::PluginError::Session(
            "invalid ingress engine program".to_owned(),
        ));
    }
    let env = env.ok_or_else(|| {
        lash_core::PluginError::Session(
            "ingress admission requires the recorded execution environment".to_string(),
        )
    })?;
    let draft = lash_core::ProcessDefinitionDraft::new(
        INGRESS_ENGINE_KIND,
        serde_json::json!({
            "payload": payload,
            "model": env.policy.profile_key().map(ToString::to_string),
        }),
        [],
    )
    .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
    let mut identity = lash_core::ProcessIdentity::labelled(
        INGRESS_ENGINE_KIND,
        payload.get("program").and_then(serde_json::Value::as_str),
    );
    identity.definition_id = Some(draft.id());
    Ok(identity)
}

struct IngressAdmissionEnginePlugin;

impl lash_core::plugin::SessionPlugin for IngressAdmissionEnginePlugin {
    fn id(&self) -> &'static str {
        "ingress-admission-engine-plugin"
    }

    fn register(
        &self,
        _reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

struct IngressAdmissionEngineFactory;

impl lash_core::plugin::PluginFactory for IngressAdmissionEngineFactory {
    fn id(&self) -> &'static str {
        "ingress-admission-engine-factory"
    }

    fn process_engine_contributions(
        &self,
        _ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> std::result::Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError>
    {
        Ok(vec![lash_core::ProcessEngineRegistration::new(
            Arc::new(IngressAdmissionEngine),
            lash_core::ProcessEngineAdmission::new(INGRESS_ENGINE_KIND, admit_ingress_engine),
        )?])
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(IngressAdmissionEnginePlugin))
    }
}

impl lash_core::plugin::PluginDefinition for IngressAdmissionEngineFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("ingress-admission-engine-factory")
    }
}

async fn ingress_engine_core(
    backend: lash_core::Backend,
) -> Result<(LashCore, Arc<dyn ProcessRegistry>)> {
    ingress_engine_core_with(backend, |builder| builder).await
}

/// [`ingress_engine_core`] with the host's own additions to its builder.
pub(super) async fn ingress_engine_core_with(
    backend: lash_core::Backend,
    configure: impl FnOnce(crate::core::LashCoreBuilder) -> crate::core::LashCoreBuilder,
) -> Result<(LashCore, Arc<dyn ProcessRegistry>)> {
    let registry = backend.process_registry();
    let core = configure(
        explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(Arc::new(IngressAdmissionEngineFactory)),
    )
    .build(crate::testing::runtime_lease_owner())?;
    core.host_artifacts()
        .publish_process_env(
            &lash_core::HostArtifactPin::mint(),
            &lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy {
                    model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                    ..lash_core::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                        crate::NoProgressBudget::bounded(12),
                    )
                },
                lash_core::SessionToolAccess::ambient(),
            ),
        )
        .await?;
    let _session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    Ok((core, registry))
}

pub(super) fn engine_start_intent(kind: &str, payload: serde_json::Value) -> lash_core::ToolIntent {
    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        owner: crate::RuntimeOwner::Session(SessionId::fixture(SESSION.to_string())),
        declaration: lash_core::ProcessStartDeclaration::new(
            lash_core::ProcessInput::Engine {
                kind: kind.to_string(),
                payload,
            },
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached,
        )
        .with_env_ref(
            (lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy {
                    model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                    ..lash_core::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                        crate::NoProgressBudget::bounded(12),
                    )
                },
                lash_core::SessionToolAccess::ambient(),
            ))
            .stable_ref()
            .expect("captured environment digest"),
        ),
    }))
}

fn ingress_engine_env_spec() -> lash_core::ProcessExecutionEnvSpec {
    lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy {
            model: Some(recorded_llm_profile(mock_llm_profile_spec())),
            ..lash_core::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
                crate::NoProgressBudget::bounded(12),
            )
        },
        lash_core::SessionToolAccess::ambient(),
    )
}

/// The host front door records engine admission too: an unconfigured engine
/// registers no process, and an admitted one carries the engine identity stamp.
#[tokio::test]
async fn ingress_start_intent_crosses_the_engine_admission_gate() -> Result<()> {
    let (core, registry) = ingress_engine_core(sqlite_memory_store_backend().await).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;

    let unregistered_key = ingress
        .key("ingress-unregistered-engine", 0)
        .expect("a host submission handle");
    let refused = ingress
        .submit(
            unregistered_key,
            engine_start_intent("ingress-engine-never-registered", serde_json::json!({})),
        )
        .await;
    match &refused {
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome:
                lash_core::ToolIntentExecutionOutcome::Refused {
                    kind: lash_core::ToolIntentKind::StartProcess,
                    refusal: lash_core::ToolIntentRefusalReason::CommandFailed { cause, .. },
                    ..
                },
            replayed: false,
        } => assert!(
            cause
                .to_string()
                .contains("process engine `ingress-engine-never-registered` is not configured"),
            "the refusal must carry the engine registry's own typed miss: {cause}"
        ),
        other => panic!("unregistered engine kind must be refused, got {other:?}"),
    }
    assert_eq!(
        registered_process_count(&registry).await?,
        0,
        "a refused start must register nothing"
    );

    let invalid = ingress
        .submit(
            ingress
                .key("ingress-invalid-engine-payload", 0)
                .expect("submission key"),
            engine_start_intent(
                INGRESS_ENGINE_KIND,
                serde_json::json!({"program": "invalid"}),
            ),
        )
        .await;
    assert!(
        matches!(
            invalid,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Refused { .. },
                ..
            }
        ),
        "a plugin-contributed engine must validate its payload: {invalid:?}"
    );
    assert_eq!(registered_process_count(&registry).await?, 0);

    let payload = serde_json::json!({"program": "known"});
    let admitted_key = ingress
        .key("ingress-registered-engine", 0)
        .expect("a host submission handle");
    let admitted = ingress
        .submit(
            admitted_key,
            engine_start_intent(INGRESS_ENGINE_KIND, payload.clone()),
        )
        .await;
    assert!(
        matches!(
            &admitted,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                    realized: lash_core::ToolIntentRealized::StartProcess(_),
                    ..
                },
                ..
            }
        ),
        "a registered engine kind must still be admitted: {admitted:?}"
    );
    let started = registry
        .get_process(&started_process_id(&admitted))
        .await?
        .expect("admitted start registers its row");
    assert_eq!(
        started.identity,
        admit_ingress_engine(
            INGRESS_ENGINE_KIND,
            &payload,
            Some(&ingress_engine_env_spec())
        )
        .expect("known payload and recorded environment"),
        "the admitted row must carry the engine identity stamp"
    );
    let replay = ingress
        .submit(
            ingress
                .key("ingress-registered-engine", 0)
                .expect("same submission key"),
            engine_start_intent(INGRESS_ENGINE_KIND, payload),
        )
        .await;
    assert!(matches!(
        replay,
        crate::tools::ToolIntentIngressOutcome::Admitted { replayed: true, .. }
    ));
    assert_eq!(started_process_id(&replay), started.id);
    assert_eq!(registered_process_count(&registry).await?, 1);
    Ok(())
}

/// FIG-1838: host ingress and session-owned recorded-intent execution must feed
/// the same immutable request environment into engine admission. Otherwise an
/// environment-derived identity changes solely with the route used to start it.
#[tokio::test]
async fn equivalent_recorded_start_has_same_environment_sensitive_identity_across_routes()
-> Result<()> {
    let (core, registry) = ingress_engine_core(sqlite_memory_store_backend().await).await?;
    let payload = serde_json::json!({"program": "environment-sensitive"});

    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, "host-ingress-route"),
    )?;
    let ingress_key = ingress
        .key("environment-sensitive-host", 0)
        .expect("a host submission handle");
    let host_outcome = ingress
        .submit(
            ingress_key,
            engine_start_intent(INGRESS_ENGINE_KIND, payload.clone()),
        )
        .await;
    assert!(
        matches!(
            host_outcome,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed { .. },
                ..
            }
        ),
        "host ingress must admit the environment-sensitive engine"
    );
    let ingress_identity = registry
        .get_process(&started_process_id(&host_outcome))
        .await?
        .expect("host ingress registers a process")
        .identity;

    let session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let effect_host = session.effect_host();
    let scoped = effect_host.scoped(lash_core::AdmittedScope::turn(
        SESSION,
        "session-recorded-intent-route",
    ))?;
    let processes = {
        let writer = session.runtime.writer();
        let runtime = writer.lock().await;
        runtime.process_service()?
    };
    let intents =
        lash_core::ToolIntents::v3(vec![engine_start_intent(INGRESS_ENGINE_KIND, payload)]);
    let realization = lash_core::testing::execute_tool_intents_with_services(
        scoped,
        processes,
        &SessionId::from(SESSION),
        &lash_core::ToolCallId::fixture("environment-sensitive-session"),
        &intents,
    )
    .await
    .map_err(lash_core::PluginError::from)?;
    let outcomes = &realization.receipt.outcomes;
    let [
        lash_core::ToolIntentExecutionOutcome::Executed {
            realized: lash_core::ToolIntentRealized::StartProcess(result),
            ..
        },
    ] = outcomes.as_slice()
    else {
        panic!("session recorded-intent route must execute: {outcomes:?}")
    };
    // The session route stages its start for the outcome commit of the call
    // that declares it: the identity is the one that commit registers.
    let [staged] = realization.store_local.as_slice() else {
        panic!(
            "the session route stages one start: {:?}",
            realization.store_local
        )
    };
    let staged = lash_core::testing::staged_start_record(staged)?
        .expect("the session route stages a process start");
    assert_eq!(staged.id, result.process_id);
    let session_identity = staged.identity;

    assert_eq!(ingress_identity, session_identity);
    let expected = lash_core::ProcessDefinitionDraft::new(
        INGRESS_ENGINE_KIND,
        serde_json::json!({
            "payload": {"program": "environment-sensitive"},
            "model": "mock-model",
        }),
        [],
    )
    .expect("the environment produces a canonical definition");
    assert_eq!(
        ingress_identity.definition_id,
        Some(expected.id()),
        "the shared identity must prove the recorded environment reached admission"
    );
    Ok(())
}
