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
