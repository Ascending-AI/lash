use super::*;

const SIGNED_ENGINE_KIND: &str = "signed-engine";

/// An engine whose stored artifact says one thing about every definition it
/// owns. It is the only authority on that signature; a reference travelling
/// with a different one is a claim, and a claim is not evidence.
struct SignedEngine;

fn authoritative_signature() -> ProcessSignature {
    ProcessSignature::known(serde_json::json!({"returns": "receipt"}))
}

fn declared_signal() -> ProcessEventType {
    ProcessEventType {
        name: "signed.progress".to_string(),
        payload_schema: crate::LashSchema::new(serde_json::json!({"type": "object"})),
        semantics: crate::ProcessEventSemanticsSpec::default(),
    }
}

#[async_trait::async_trait]
impl ProcessEngine for SignedEngine {
    fn kind(&self) -> &'static str {
        SIGNED_ENGINE_KIND
    }

    async fn run(
        &self,
        _context: ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<ProcessRunOutcome, ProcessInfraError> {
        unreachable!("resolution never runs the process")
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
        Ok(ProcessDefinitionResolution::new(
            authoritative_signature(),
            vec![declared_signal()],
        ))
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

/// A reference that asserts nothing adopts the engine's authority, and the
/// signals the definition declares ride the admission instead of being
/// re-declared by the caller.
#[tokio::test]
async fn an_unclaimed_reference_adopts_the_artifact_signature_and_signals() {
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
    assert_eq!(admitted.signals(), [declared_signal()]);
}

/// The load-bearing refusal: a fabricated signature never reaches a process
/// row. `admit` is the only path from a recorded engine payload to a
/// registration identity, and it fails before any registration exists.
#[tokio::test]
async fn a_fabricated_signature_is_refused_before_a_row_can_exist() {
    let refusal = registry()
        .admit(
            SIGNED_ENGINE_KIND,
            &serde_json::json!({"program": "payout", "signature": {"returns": "anything"}}),
            None,
        )
        .await
        .expect_err("a claim the artifact disagrees with is refused");

    let message = refusal.to_string();
    assert!(
        message.contains("disagrees"),
        "the refusal names the disagreement: {message}"
    );
    assert!(
        message.contains(SIGNED_ENGINE_KIND),
        "the refusal names the engine that holds the authority: {message}"
    );
}

/// A claim that happens to be true is not a second encoding of the truth: it
/// is admitted, and what is stored is still the resolved authority.
#[tokio::test]
async fn a_truthful_claim_is_admitted() {
    let admitted = registry()
        .admit(
            SIGNED_ENGINE_KIND,
            &serde_json::json!({"program": "payout", "signature": {"returns": "receipt"}}),
            None,
        )
        .await
        .expect("a claim equal to the authority is admitted");
    assert_eq!(
        admitted
            .identity()
            .definition
            .as_ref()
            .expect("definition reference")
            .signature,
        authoritative_signature()
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

/// The fingerprint names the definition, never the claim about it: two
/// references that disagree only on the signature name the same definition, so
/// a forged claim cannot silently become a different process.
#[test]
fn a_signature_claim_does_not_move_the_fingerprint() {
    let unclaimed = ProcessDefinitionRef::unclaimed(
        SIGNED_ENGINE_KIND,
        serde_json::json!({"program": "payout"}),
    );
    let forged = ProcessDefinitionRef::new(
        SIGNED_ENGINE_KIND,
        serde_json::json!({"program": "payout"}),
        ProcessSignature::known(serde_json::json!({"returns": "anything"})),
    );
    assert_eq!(unclaimed.fingerprint(), forged.fingerprint());
    assert!(unclaimed.names_same_definition(&forged));
}

/// FIG-1522: trigger registration admits its engine target through the same
/// registry boundary a start does. A subscription naming an engine kind this
/// host never registered is refused at registration with the registry's typed
/// `UnknownEngine` refusal — not accepted and then discovered dead at the first
/// delivery, when the registrant is gone and the failure is invisible.
#[tokio::test]
async fn trigger_registration_refuses_an_unregistered_engine_kind() {
    let reference = ProcessDefinitionRef::unclaimed(
        "never-registered",
        serde_json::json!({"program": "payout"}),
    );
    let mut draft = crate::TriggerSubscriptionDraft::for_process(
        "sub",
        crate::ProcessExecutionEnvRef::new("env"),
        "app.event",
        "key",
        crate::ProcessInput::Engine {
            kind: "never-registered".to_string(),
            payload: serde_json::json!({"program": "payout"}),
        },
        crate::ProcessIdentity::for_definition(reference, None::<String>),
    );

    let refusal = crate::admit_trigger_registration_target(&registry(), &mut draft)
        .await
        .expect_err("no engine owns that kind, so the registration is refused");

    let message = refusal.to_string();
    assert!(
        message.contains("never-registered"),
        "the refusal names the unregistered engine kind: {message}"
    );
    assert!(
        message.contains("engine"),
        "the refusal is the registry's typed unknown-engine answer: {message}"
    );
}

/// The same boundary pins the authority on the way through: a registration
/// naming a registered engine keeps its target and leaves registration with the
/// artifact's signature, not the unknown claim that arrived.
#[tokio::test]
async fn trigger_registration_pins_the_resolved_signature_on_the_target() {
    let mut draft = crate::TriggerSubscriptionDraft::for_process(
        "sub",
        crate::ProcessExecutionEnvRef::new("env"),
        "app.event",
        "key",
        crate::ProcessInput::Engine {
            kind: SIGNED_ENGINE_KIND.to_string(),
            payload: serde_json::json!({"program": "payout"}),
        },
        crate::ProcessIdentity::for_definition(
            ProcessDefinitionRef::unclaimed(
                SIGNED_ENGINE_KIND,
                serde_json::json!({"program": "payout"}),
            ),
            None::<String>,
        ),
    );

    crate::admit_trigger_registration_target(&registry(), &mut draft)
        .await
        .expect("a registered engine admits the registration target");

    assert_eq!(
        draft
            .target_identity
            .definition
            .as_ref()
            .expect("the admitted target pins a definition reference")
            .signature,
        authoritative_signature(),
        "registration stores the engine's authority, never the unknown claim"
    );
}

/// FIG-3122 law (a): a label the host declared for a start survives admission
/// byte-identical, and law (c): declaring it moves nothing else on the row.
///
/// Admission stays the sole writer of a registration's derived identity — the
/// kind and the definition reference only the engine can resolve. The label is
/// not derived identity, it is display metadata, so the engine's own label is
/// the default a start that declared none falls back to, never an override of
/// the name a caller asked for.
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
            crate::ProcessId::from("process-label-law"),
            crate::ProcessInput::Engine {
                kind: SIGNED_ENGINE_KIND.to_string(),
                payload: serde_json::json!({"program": "payout"}),
            },
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::host(),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
    };

    let undeclared = registration().with_admitted_identity(admitted());
    assert_eq!(
        undeclared.identity.label.as_deref(),
        Some(engine_label),
        "a start that declares no label keeps the one the engine derived"
    );

    let declared = registration()
        .with_declared_identity(crate::DeclaredProcessIdentity::labelled(
            SIGNED_ENGINE_KIND,
            Some("immutable_deployment_probe"),
        ))
        .with_admitted_identity(admitted());
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
