use super::*;
use lash_execution_contracts::process as contracts;
use lash_execution_contracts::process::ProcessRegistration;
use lash_execution_contracts::process::ProcessSignature;

const SIGNED_ENGINE_KIND: &str = "signed-engine";

/// An engine whose stored artifact says one thing about every definition it
/// owns. It is the only authority on that signature; a reference travelling
/// with a different one is a claim, and a claim is not evidence.
struct SignedEngine;

fn authoritative_signature() -> ProcessSignature {
    ProcessSignature::known(serde_json::json!({"returns": "receipt"}))
}

#[async_trait::async_trait]
impl ProcessEngine for SignedEngine {
    fn settled_ran(
        &self,
        _kind: &lash_execution_contracts::process::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Internal
    }

    async fn performed_effects(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<lash_execution_contracts::process::PerformedEffects, crate::ProcessEnvRefusal> {
        Ok(lash_execution_contracts::process::PerformedEffects::new())
    }

    async fn check_args(
        &self,
        _signature: &lash_execution_contracts::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_execution_contracts::process::ArgsMode,
    ) -> std::result::Result<(), lash_execution_contracts::process::ArgsMismatch> {
        Err(
            lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: self.kind().into(),
            },
        )
    }

    fn kind(&self) -> &'static str {
        SIGNED_ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, lash_execution_contracts::plugin_error::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        _artifact_ref: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        unreachable!("resolution never starts the process")
    }

    async fn resolve(
        &self,
        reference: &ProcessDefinitionRef,
    ) -> Result<
        ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        if reference.definition.as_json().get("program").is_none() {
            return Err(ProcessDefinitionRefusal::UnresolvableDefinition {
                engine_kind: reference.engine_kind.clone(),
                message: "definition names no program".to_string(),
            }
            .into());
        }
        Ok(ProcessDefinitionResolution::new(authoritative_signature()))
    }

    fn state_format(&self) -> lash_execution_contracts::process::EngineStateFormat {
        lash_execution_contracts::process::EngineStateFormat {
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
    ) -> Option<crate::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine `{}` takes no settings",
                    self.kind()
                )),
            ),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(None)
    }

    fn advance(
        &self,
        _state: lash_execution_contracts::process::EngineState,
        _event: lash_execution_contracts::process::EngineEvent,
    ) -> Result<
        (
            lash_execution_contracts::process::EngineState,
            lash_execution_contracts::process::EngineAction,
        ),
        lash_execution_contracts::process::ProcessInfraError,
    > {
        unreachable!("resolution never advances the process")
    }
}

/// The admission policy is pure: it derives the reference the recorded payload
/// names, including whatever signature the payload claims. It cannot read an
/// artifact, so it never checks the claim — the registry does.
fn admit_signed(
    kind: &'static str,
    payload: &serde_json::Value,
) -> Result<ProcessIdentity, lash_execution_contracts::plugin_error::PluginError> {
    let signature = match payload.get("signature") {
        Some(claim) => ProcessSignature::known(claim.clone()),
        None => ProcessSignature::Unknown,
    };
    Ok(ProcessIdentity::for_definition(
        ProcessDefinitionRef::new(
            kind,
            serde_json::json!({"program": payload.get("program").cloned()}),
            signature,
        ),
        None::<String>,
    ))
}

fn registry() -> ProcessEngineRegistry {
    ProcessEngineRegistry::new().with_registration(
        ProcessEngineRegistration::new(
            Arc::new(SignedEngine) as Arc<dyn ProcessEngine>,
            ProcessEngineAdmission::new(SIGNED_ENGINE_KIND, admit_signed),
        )
        .expect("engine and admission agree on the kind"),
    )
}

/// A reference that asserts nothing adopts the engine's authority.
#[tokio::test]
async fn an_unclaimed_reference_adopts_the_artifact_signature() {
    let admitted = registry()
        .admit(
            SIGNED_ENGINE_KIND,
            &serde_json::json!({"program": "payout"}),
        )
        .await
        .expect("an unclaimed reference is admitted");

    let reference = admitted
        .identity()
        .definition
        .as_ref()
        .expect("the admitted identity pins a definition reference");
    assert_eq!(
        reference.signature,
        authoritative_signature(),
        "the durable row pins the engine's authority, never the claim that arrived"
    );
}

/// Typed refusals, not stringly-typed ones: an engine that cannot resolve the
/// definition says so, and an engine nobody registered is a distinct answer
/// from an engine that refused.
#[tokio::test]
async fn unresolvable_and_unknown_engines_are_distinct_refusals() {
    let registry = registry();
    let unresolvable = registry
        .resolve(&ProcessDefinitionRef::unclaimed(
            SIGNED_ENGINE_KIND,
            serde_json::json!({"nothing": true}),
        ))
        .await
        .expect_err("the engine cannot resolve a definition naming no program");
    assert!(matches!(
        unresolvable.refusal(),
        Some(ProcessDefinitionRefusal::UnresolvableDefinition { .. })
    ));

    assert_eq!(
        unresolvable.refusal().unwrap().code(),
        "unresolvable_definition"
    );
    let unknown = registry
        .resolve(&ProcessDefinitionRef::unclaimed(
            "never-registered",
            serde_json::json!({"program": "payout"}),
        ))
        .await
        .expect_err("no engine owns that kind");
    assert!(matches!(
        unknown.refusal(),
        Some(ProcessDefinitionRefusal::UnknownEngine { .. })
    ));
    assert_eq!(unknown.refusal().unwrap().code(), "unknown_engine");
}

/// ADR 0013: an engine kind is registered once on a runtime host. The
/// registry's enforcement point refuses a second registration under a kind it
/// already holds, whichever way the engine arrived.
#[test]
fn a_duplicate_engine_kind_is_refused() {
    let registry = registry();
    let refusal = match registry
        .clone()
        .try_with_engine(ProcessEngineRegistration::accepting(
            Arc::new(SignedEngine) as Arc<dyn ProcessEngine>
        )) {
        Err(refusal) => refusal,
        Ok(_) => panic!("a second engine under a held kind is refused"),
    };
    assert!(
        matches!(
            refusal,
            lash_execution_contracts::plugin_error::PluginError::Registration(_)
        ),
        "the refusal is a registration error: {refusal}"
    );
    assert_eq!(
        refusal.to_string(),
        format!(
            "plugin registration error: duplicate process engine kind `{SIGNED_ENGINE_KIND}`; each engine kind may be registered once"
        )
    );
    assert!(
        registry.get(SIGNED_ENGINE_KIND).is_some(),
        "the refused registration replaces nothing"
    );
}

/// FIG-3122 law (a): a label the host declared for a start reaches the row
/// byte-identical, and law (c): declaring it moves nothing else on the row.
///
/// Admission stays the sole writer of a registration's derived identity — the
/// kind, the definition reference only the engine can resolve, and the label it
/// derives. The declared label is restored over that derived one afterwards,
/// from the declaration that carried it; a start that declared none keeps the
/// engine's. The restoration is deliberately not a decision `with_admitted_identity`
/// makes by inspecting the row: a label already on a registration is not evidence
/// that a host declared it, and treating it as evidence let a first admitted stamp
/// mask a second (#1543, the `process_roster_pages_match_enriched_filters` law).
#[tokio::test]
async fn a_declared_label_survives_the_admitted_stamp_byte_identical() {
    let engine_label = "__process_02178275819fb79b903c9a8b03a8b2d28c41708383b1728900e429e3a59b6a32";
    let admitted = || {
        lash_execution_contracts::process::AdmittedProcessIdentity::for_testing(
            ProcessIdentity::for_definition(
                ProcessDefinitionRef::new(
                    SIGNED_ENGINE_KIND,
                    serde_json::json!({"program": "payout"}),
                    authoritative_signature(),
                ),
                Some(engine_label),
            ),
        )
    };
    let registration = || {
        ProcessRegistration::new(
            lash_execution_contracts::process::ProcessInput::Engine {
                kind: SIGNED_ENGINE_KIND.to_string(),
                payload: serde_json::json!({"program": "payout"}),
                launch: lash_execution_contracts::testing::fixture_engine_launch(),
            },
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        )
    };

    let undeclared = registration()
        .with_admitted_identity(admitted())
        .with_host_facing_label(None);
    assert_eq!(
        undeclared.identity.label.as_deref(),
        Some(engine_label),
        "a start that declares no label keeps the one the engine derived"
    );

    // A second admitted stamp is the engine speaking again, not a declaration:
    // it replaces the label outright. This is the regression #1543 shipped.
    let restamped = registration()
        .with_admitted_identity(
            lash_execution_contracts::process::AdmittedProcessIdentity::for_testing(
                ProcessIdentity::labelled(SIGNED_ENGINE_KIND, Some("first-stamp")),
            ),
        )
        .with_admitted_identity(admitted());
    assert_eq!(
        restamped.identity.label.as_deref(),
        Some(engine_label),
        "an earlier admitted label never masks a later admitted one"
    );

    let declared = registration()
        .with_declared_identity(
            lash_execution_contracts::process::DeclaredProcessIdentity::labelled(
                SIGNED_ENGINE_KIND,
                Some("immutable_deployment_probe"),
            ),
        )
        .with_admitted_identity(admitted())
        .with_host_facing_label(Some("immutable_deployment_probe".to_string()));
    assert_eq!(
        declared.identity.label.as_deref(),
        Some("immutable_deployment_probe"),
        "the declared label is what the row carries, not the engine's derived one"
    );
    assert_eq!(
        declared.identity.kind, undeclared.identity.kind,
        "a label never moves the engine kind"
    );
    assert_eq!(
        declared.identity.definition, undeclared.identity.definition,
        "a label never moves the definition reference: it is display metadata, not an identity input"
    );
}

/// Every optional capability a registration can declare; none is ever run.
struct Capabilities;

const CAPABILITY_STEP: &str = "capability-step";

#[async_trait::async_trait]
impl contracts::EngineSteps for Capabilities {
    fn kinds(&self) -> Vec<contracts::EngineStepKind> {
        vec![contracts::EngineStepKind::new(CAPABILITY_STEP)]
    }

    fn execution(&self, _kind: &contracts::EngineStepKind) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }

    fn retry(&self, _kind: &contracts::EngineStepKind) -> lash_protocol_types::ExecutionPolicy {
        lash_protocol_types::ExecutionPolicy::default()
    }

    async fn run(
        &self,
        _run: contracts::EngineStepRun,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> contracts::EngineStepAnswer {
        unreachable!("the registry never runs a step")
    }
}

#[async_trait::async_trait]
impl contracts::EngineStateMigration for Capabilities {
    fn carries(&self) -> Vec<contracts::EngineStateFormat> {
        Vec::new()
    }

    async fn migrate(
        &self,
        _process: &contracts::ProcessId,
        _state: &contracts::EngineState,
    ) -> Result<Option<contracts::EngineState>, contracts::EngineStateRefusal> {
        unreachable!("the registry never migrates")
    }
}

#[async_trait::async_trait]
impl contracts::EngineSuccession for Capabilities {
    async fn carry(
        &self,
        _process: &contracts::ProcessId,
        _from: &serde_json::Value,
        _to: &serde_json::Value,
        _state: Option<&contracts::EngineState>,
        _environment: &lash_execution_contracts::process::ProcessEnv,
    ) -> Result<contracts::SuccessorCarry, contracts::SuccessionError> {
        unreachable!("the registry never carries")
    }
}

impl contracts::EngineStatusProjection for Capabilities {
    fn status(&self, _state: &contracts::EngineState) -> Option<serde_json::Value> {
        None
    }
}

fn fully_declared() -> ProcessEngineRegistration {
    let capabilities = Arc::new(Capabilities);
    ProcessEngineRegistration::accepting(Arc::new(SignedEngine) as Arc<dyn ProcessEngine>)
        .with_engine_steps(capabilities.clone())
        .with_state_migration(capabilities.clone())
        .with_succession(capabilities.clone())
        .with_status_projection(capabilities)
}

/// Which optional capabilities `registry` serves for the signed engine.
fn declared(registry: &ProcessEngineRegistry) -> [bool; 4] {
    [
        registry
            .engine_steps(
                SIGNED_ENGINE_KIND,
                &contracts::EngineStepKind::new(CAPABILITY_STEP),
            )
            .is_ok(),
        registry.state_migrations().next().is_some(),
        registry.succession(SIGNED_ENGINE_KIND).is_some(),
        registry.status_projections().next().is_some(),
    ]
}

/// Replacing an engine kind's registration replaces it whole: every
/// capability the new registration omits is withdrawn with the old one.
#[test]
fn a_replacing_registration_withdraws_every_capability_it_omits() {
    let declaring = ProcessEngineRegistry::new().with_registration(fully_declared());
    assert_eq!(declared(&declaring), [true; 4]);

    let replaced = declaring
        .with_registration(ProcessEngineRegistration::accepting(
            Arc::new(SignedEngine) as Arc<dyn ProcessEngine>
        ));

    assert_eq!(declared(&replaced), [false; 4]);
    assert!(
        matches!(
            replaced.engine_steps(
                SIGNED_ENGINE_KIND,
                &contracts::EngineStepKind::new(CAPABILITY_STEP)
            ),
            Err(contracts::EngineStepRefusal::NoEngineSteps { engine })
                if engine == SIGNED_ENGINE_KIND
        ),
        "a known engine without steps is not an unknown engine"
    );
}

/// A weak registry upgrades to the snapshot it was taken from while a holder
/// keeps that snapshot, and keeps nothing alive on its own.
#[test]
fn a_weak_registry_reads_its_own_snapshot_and_keeps_none_alive() {
    let declaring = ProcessEngineRegistry::new().with_registration(fully_declared());
    let weak = declaring.downgrade();
    let replaced = declaring
        .clone()
        .with_registration(ProcessEngineRegistration::accepting(
            Arc::new(SignedEngine) as Arc<dyn ProcessEngine>
        ));

    let upgraded = weak.upgrade().expect("the declaring registry is held");
    assert_eq!(declared(&upgraded), [true; 4]);
    assert_eq!(declared(&replaced), [false; 4]);

    drop((upgraded, declaring));
    assert!(
        weak.upgrade().is_none(),
        "the weak registry keeps no engine alive once its holders drop"
    );
}
