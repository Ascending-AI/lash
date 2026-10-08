use super::*;
use crate::ProcessRegistration;
use crate::ProcessSignature;

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
    async fn check_args(
        &self,
        _signature: &crate::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: crate::ArgsMode,
    ) -> std::result::Result<(), crate::ArgsMismatch> {
        Err(crate::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

    fn kind(&self) -> &'static str {
        SIGNED_ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        _artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        unreachable!("resolution never starts the process")
    }

    async fn resolve(
        &self,
        reference: &ProcessDefinitionRef,
    ) -> Result<ProcessDefinitionResolution, ProcessDefinitionRefusal> {
        if reference.definition.as_json().get("program").is_none() {
            return Err(ProcessDefinitionRefusal::UnresolvableDefinition {
                engine_kind: reference.engine_kind.clone(),
                message: "definition names no program".to_string(),
            });
        }
        Ok(ProcessDefinitionResolution::new(authoritative_signature()))
    }

    fn state_format(&self) -> crate::EngineStateFormat {
        crate::EngineStateFormat {
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

    fn creation_config(
        &self,
        _env_spec: &crate::ProcessExecutionEnvSpec,
    ) -> Result<Option<serde_json::Value>, crate::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        _state: crate::EngineState,
        _event: crate::EngineEvent,
    ) -> Result<(crate::EngineState, crate::EngineAction), crate::ProcessInfraError> {
        unreachable!("resolution never advances the process")
    }
}

/// The admission policy is pure: it derives the reference the recorded payload
/// names, including whatever signature the payload claims. It cannot read an
/// artifact, so it never checks the claim — the registry does.
fn admit_signed(
    kind: &'static str,
    payload: &serde_json::Value,
    _env: Option<&ProcessExecutionEnvSpec>,
) -> Result<ProcessIdentity, crate::PluginError> {
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
            None,
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
        unresolvable,
        ProcessDefinitionRefusal::UnresolvableDefinition { .. }
    ));

    let unknown = registry
        .resolve(&ProcessDefinitionRef::unclaimed(
            "never-registered",
            serde_json::json!({"program": "payout"}),
        ))
        .await
        .expect_err("no engine owns that kind");
    assert!(matches!(
        unknown,
        ProcessDefinitionRefusal::UnknownEngine { .. }
    ));
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
        matches!(refusal, crate::PluginError::Registration(_)),
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
/// mask a second (#1543, the `list_processes_filters_by_enriched_fields` law).
#[tokio::test]
async fn a_declared_label_survives_the_admitted_stamp_byte_identical() {
    let engine_label = "__process_02178275819fb79b903c9a8b03a8b2d28c41708383b1728900e429e3a59b6a32";
    let admitted = || {
        crate::AdmittedProcessIdentity::for_testing(ProcessIdentity::for_definition(
            ProcessDefinitionRef::new(
                SIGNED_ENGINE_KIND,
                serde_json::json!({"program": "payout"}),
                authoritative_signature(),
            ),
            Some(engine_label),
        ))
    };
    let registration = || {
        ProcessRegistration::new(
            crate::ProcessInput::Engine {
                kind: SIGNED_ENGINE_KIND.to_string(),
                payload: serde_json::json!({"program": "payout"}),
            },
            crate::ProcessProvenance::host(),
            crate::Lifetime::Detached,
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
        .with_admitted_identity(crate::AdmittedProcessIdentity::for_testing(
            ProcessIdentity::labelled(SIGNED_ENGINE_KIND, Some("first-stamp")),
        ))
        .with_admitted_identity(admitted());
    assert_eq!(
        restamped.identity.label.as_deref(),
        Some(engine_label),
        "an earlier admitted label never masks a later admitted one"
    );

    let declared = registration()
        .with_declared_identity(crate::DeclaredProcessIdentity::labelled(
            SIGNED_ENGINE_KIND,
            Some("immutable_deployment_probe"),
        ))
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
