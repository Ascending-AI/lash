use std::sync::Arc;

use super::*;
use crate::runtime::process::definition_ref::ProcessDefinitionResolution;
use crate::runtime::process::engine::{
    ProcessEngine, ProcessEngineAdmission, ProcessEngineRegistration,
};
use crate::{ProcessExecutionEnvSpec, ProcessIdentity};

const SIGNED_ENGINE_KIND: &str = "signed-engine";

fn module(artifact_ref: &str) -> ArtifactName {
    ArtifactName {
        store: ArtifactStoreId::LashlangModule,
        artifact_ref: artifact_ref.to_string(),
    }
}

fn env(artifact_ref: &str) -> ArtifactName {
    ArtifactName {
        store: ArtifactStoreId::ProcessEnv,
        artifact_ref: artifact_ref.to_string(),
    }
}

fn engine_blob(kind: &str, artifact_ref: &str) -> ArtifactName {
    ArtifactName {
        store: ArtifactStoreId::Engine(kind.to_string()),
        artifact_ref: artifact_ref.to_string(),
    }
}

fn draft(
    engine_kind: &str,
    value: serde_json::Value,
    artifacts: impl IntoIterator<Item = ArtifactName>,
) -> ProcessDefinitionDraft {
    ProcessDefinitionDraft::new(engine_kind, value, artifacts).expect("a well-formed draft")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// An engine whose stored artifact is the only authority on the signature of
/// every definition it owns.
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

    /// A program reads the module named after it.
    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(payload
            .get("program")
            .and_then(serde_json::Value::as_str)
            .map(|program| module(&format!("module:{program}")))
            .into_iter()
            .collect())
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
        unreachable!("deriving a definition never acquires an artifact")
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
        unreachable!("deriving a definition never advances a process")
    }
}

fn admit_signed(
    kind: &'static str,
    payload: &serde_json::Value,
    _env: Option<&ProcessExecutionEnvSpec>,
) -> Result<ProcessIdentity, crate::PluginError> {
    Ok(ProcessIdentity::for_definition(
        ProcessDefinitionRef::unclaimed(kind, payload.clone()),
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

fn signed_draft() -> ProcessDefinitionDraft {
    draft(
        SIGNED_ENGINE_KIND,
        serde_json::json!({"program": "payout"}),
        [module("module:payout")],
    )
}

/// Any changed byte of the engine kind, the canonical value or the artifact
/// set names a different definition, and no two of these variants collide.
#[test]
fn changed_content_changes_the_id() {
    let base = || {
        draft(
            "lashlang",
            serde_json::json!({"program": "scan", "limit": 1}),
            [module("module:1"), env("env:1")],
        )
    };
    let variants = [
        base(),
        draft(
            "lashlang-next",
            serde_json::json!({"program": "scan", "limit": 1}),
            [module("module:1"), env("env:1")],
        ),
        draft(
            "lashlang",
            serde_json::json!({"program": "scan", "limit": 2}),
            [module("module:1"), env("env:1")],
        ),
        draft(
            "lashlang",
            serde_json::json!({"program": "scan", "limit": 1.0}),
            [module("module:1"), env("env:1")],
        ),
        draft(
            "lashlang",
            serde_json::json!({"program": "scan", "limit": 1}),
            [module("module:2"), env("env:1")],
        ),
        draft(
            "lashlang",
            serde_json::json!({"program": "scan", "limit": 1}),
            [module("module:1")],
        ),
        draft(
            "lashlang",
            serde_json::json!({"program": "scan", "limit": 1}),
            [module("module:1"), env("env:1"), module("module:3")],
        ),
        // The same reference text in a different store is different content.
        draft(
            "lashlang",
            serde_json::json!({"program": "scan", "limit": 1}),
            [env("module:1"), env("env:1")],
        ),
        draft(
            "lashlang",
            serde_json::json!({"program": "scan", "limit": 1}),
            [engine_blob("lashlang", "module:1"), env("env:1")],
        ),
        // Framing: moving bytes between the kind and the value must not
        // produce the same preimage.
        draft(
            "lashlang\"",
            serde_json::json!({"program": "scan", "limit": 1}),
            [module("module:1"), env("env:1")],
        ),
    ];

    let ids = variants
        .iter()
        .map(ProcessDefinitionDraft::id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        ids.len(),
        variants.len(),
        "every changed content has its own id"
    );
    assert_eq!(
        base().id(),
        variants[0].id(),
        "unchanged content keeps its id"
    );
}

/// The signature is derived by the owning engine, not hashed: a held
/// definition whose signature claim disagrees with the derivation is refused,
/// and its id is the same whatever it claims.
#[tokio::test]
async fn forged_signature_claim_is_refused() {
    let registry = registry();
    let draft = signed_draft();

    let derived = registry
        .derive_definition(&draft)
        .await
        .expect("the engine derives the signature");
    assert_eq!(derived.id, draft.id());
    assert_eq!(derived.signature, authoritative_signature());

    let forged = ProcessDefinition::new(
        draft.id(),
        ProcessSignature::known(serde_json::json!({"returns": "anything"})),
    );
    let refusal = registry
        .verify_definition_claim(&draft, &forged)
        .await
        .expect_err("a forged signature claim is refused");
    assert_eq!(
        refusal,
        ProcessDefinitionRefusal::SignatureMismatch {
            engine_kind: ProcessEngineKind::from(SIGNED_ENGINE_KIND),
            claimed: forged.signature.clone(),
            authoritative: authoritative_signature(),
        }
    );

    assert_eq!(
        registry
            .verify_definition_claim(&draft, &derived)
            .await
            .expect("a truthful claim is admitted"),
        derived
    );
    assert_eq!(
        registry
            .verify_definition_claim(
                &draft,
                &ProcessDefinition::new(draft.id(), ProcessSignature::Unknown)
            )
            .await
            .expect("an unknown claim adopts the derivation"),
        derived
    );

    // A claim cannot move an id onto other content either.
    let other = ProcessDefinitionDraft::new(
        SIGNED_ENGINE_KIND,
        serde_json::json!({"program": "refund"}),
        [module("module:payout")],
    )
    .expect("a well-formed draft");
    let refusal = registry
        .verify_definition_claim(&other, &derived)
        .await
        .expect_err("the descriptor is not the claimed id's");
    assert_eq!(
        refusal,
        ProcessDefinitionRefusal::DefinitionIdMismatch {
            claimed: draft.id(),
            derived: other.id(),
        }
    );
}

/// One encoding: the id is the tagged record in every JSON position — alone,
/// inside a definition, inside either target — its display is the spelling a
/// column holds, and the id core uses is the type the protocol layer defines.
#[test]
fn one_definition_id_encoding_everywhere() {
    let id = signed_draft().id();
    let spelling = id.to_string();
    let tagged = serde_json::json!({ lash_sansio::DEFINITION_ID_FIELD: spelling });

    let _: &lash_sansio::ProcessDefinitionId = &id;
    assert!(spelling.starts_with(lash_sansio::DEFINITION_ID_PREFIX));
    assert_eq!(ProcessDefinitionId::parse(&spelling).as_ref(), Ok(&id));
    assert_eq!(serde_json::to_value(&id).expect("serialize id"), tagged);
    assert_eq!(id.to_tagged_json(), tagged);
    assert_eq!(
        ProcessDefinitionId::from_tagged_json(&tagged).as_ref(),
        Ok(&id)
    );

    let definition = ProcessDefinition::new(id.clone(), authoritative_signature());
    let definition_json = serde_json::json!({
        "id": tagged,
        "signature": {"signature": "known", "encoding": {"returns": "receipt"}},
    });
    assert_eq!(
        serde_json::to_value(&definition).expect("serialize definition"),
        definition_json
    );
    assert_eq!(
        serde_json::from_value::<ProcessDefinition>(definition_json.clone())
            .expect("deserialize definition"),
        definition
    );

    let by_definition = ProcessDefinitionTarget::Definition(definition.clone());
    let by_id = ProcessDefinitionTarget::DefinitionId(id.clone());
    let by_definition_json = serde_json::json!({ "definition": definition_json });
    let by_id_json = serde_json::json!({ "definition_id": tagged });
    assert_eq!(
        serde_json::to_value(&by_definition).expect("serialize target"),
        by_definition_json
    );
    assert_eq!(
        serde_json::to_value(&by_id).expect("serialize target"),
        by_id_json
    );
    for (json, target) in [(&by_definition_json, &by_definition), (&by_id_json, &by_id)] {
        let decoded = serde_json::from_value::<ProcessDefinitionTarget>(json.clone())
            .expect("deserialize target");
        assert_eq!(&decoded, target);
        assert_eq!(decoded.definition_id(), &id);
    }
    assert_eq!(by_id.signature_claim(), &ProcessSignature::Unknown);
    assert_eq!(by_definition.signature_claim(), &authoritative_signature());

    // A bare string is text, never an id, in any position.
    for refused in [
        serde_json::json!({ "definition_id": spelling }),
        serde_json::json!({ "definition": {"id": spelling, "signature": {"signature": "unknown"}} }),
        serde_json::json!({ "definition_id": tagged, "definition": definition_json }),
        serde_json::json!({}),
        serde_json::json!({ "definition_id": tagged, "name": "scan" }),
    ] {
        assert!(
            serde_json::from_value::<ProcessDefinitionTarget>(refused.clone()).is_err(),
            "{refused}"
        );
    }
    assert!(
        serde_json::from_value::<ProcessDefinition>(serde_json::json!({
            "id": tagged,
            "signature": {"signature": "unknown"},
            "name": "scan",
        }))
        .is_err(),
        "a definition refuses unknown fields"
    );
}

/// Golden preimages, computed by an implementation independent of the
/// identity encoder (SHA-256 over hand-framed bytes). A change here is a new
/// identity family, never an edit.
#[test]
fn definition_id_golden_vectors_are_frozen() {
    let with_artifacts = draft(
        "lashlang",
        serde_json::from_str(r#"{"b":[1,-0.0],"a":"x"}"#).expect("parse value"),
        [
            module("module:1"),
            env("env:1"),
            engine_blob("scripted", "blob:9"),
            module("module:1"),
        ],
    );
    assert_eq!(
        hex(&with_artifacts.canonical_preimage()),
        concat!(
            // "lash-stable-identity", salt 2, family version 1
            "6c6173682d737461626c652d6964656e74697479",
            "02",
            "01",
            // "lash.process-definition-id"
            "000000000000001a",
            "6c6173682e70726f636573732d646566696e6974696f6e2d6964",
            // engine kind "lashlang"
            "0000000000000008",
            "6c6173686c616e67",
            // canonical value {"a":"x","b":[1,0.0]}
            "0000000000000015",
            "7b2261223a2278222c2262223a5b312c302e305d7d",
            // three artifacts, sorted by leaf bytes
            "0000000000000003",
            // process_env "env:1"
            "000000000000000e",
            "01",
            "0000000000000005",
            "656e763a31",
            // lashlang_module "module:1"
            "0000000000000011",
            "02",
            "0000000000000008",
            "6d6f64756c653a31",
            // engine "scripted" "blob:9"
            "000000000000001f",
            "03",
            "0000000000000008",
            "7363726970746564",
            "0000000000000006",
            "626c6f623a39",
        )
    );
    assert_eq!(
        with_artifacts.id().as_str(),
        "lash.definition:sha256:2052a3fc070f8684cdac78b7254bd862320cb21669c3baa5cbcb78da060b0c57"
    );

    let bare = draft("scripted-engine", serde_json::json!({}), []);
    assert_eq!(
        hex(&bare.canonical_preimage()),
        concat!(
            "6c6173682d737461626c652d6964656e74697479",
            "02",
            "01",
            "000000000000001a",
            "6c6173682e70726f636573732d646566696e6974696f6e2d6964",
            "000000000000000f",
            "73637269707465642d656e67696e65",
            "0000000000000002",
            "7b7d",
            "0000000000000000",
        )
    );
    assert_eq!(
        bare.id().as_str(),
        "lash.definition:sha256:7a20a0ebea1d71f890b6b5935ebfb1f6353c13d491e0f588fa98bf3216a252c4"
    );

    // The definition store's tag is 4, after the three frozen ones.
    let naming_a_descriptor = draft(
        "scripted-engine",
        serde_json::json!({}),
        [ArtifactName {
            store: ArtifactStoreId::ProcessDefinition,
            artifact_ref: "def:1".to_string(),
        }],
    );
    assert_eq!(
        hex(&naming_a_descriptor.canonical_preimage()),
        concat!(
            "6c6173682d737461626c652d6964656e74697479",
            "02",
            "01",
            "000000000000001a",
            "6c6173682e70726f636573732d646566696e6974696f6e2d6964",
            "000000000000000f",
            "73637269707465642d656e67696e65",
            "0000000000000002",
            "7b7d",
            "0000000000000001",
            // process_definition "def:1"
            "000000000000000e",
            "04",
            "0000000000000005",
            "6465663a31",
        )
    );
    assert_eq!(
        naming_a_descriptor.id().as_str(),
        "lash.definition:sha256:159144ab18b2e24c97e4a600274065478f4532f854ed37c114b8c53793d87e29"
    );
}

/// A manifest is exactly what the owning engine resolves the value to: a
/// descriptor that names less, more or other artifacts is refused, typed,
/// before anything is derived from it.
#[tokio::test]
async fn a_manifest_that_disagrees_with_the_engine_is_refused() {
    let registry = registry();
    registry
        .derive_definition(&signed_draft())
        .await
        .expect("the engine's own manifest is admitted");
    for artifacts in [
        Vec::new(),
        vec![module("module:refund")],
        vec![module("module:payout"), env("env:1")],
    ] {
        let declared = draft(
            SIGNED_ENGINE_KIND,
            serde_json::json!({"program": "payout"}),
            artifacts,
        );
        assert_eq!(
            registry.derive_definition(&declared).await,
            Err(ProcessDefinitionRefusal::ManifestMismatch {
                engine_kind: ProcessEngineKind::from(SIGNED_ENGINE_KIND),
                declared: declared.artifacts().to_vec(),
                resolved: vec![module("module:payout")],
            })
        );
        assert!(matches!(
            registry
                .verify_definition_claim(
                    &declared,
                    &ProcessDefinition::new(declared.id(), ProcessSignature::Unknown)
                )
                .await,
            Err(ProcessDefinitionRefusal::ManifestMismatch { .. })
        ));
    }
    assert_eq!(
        registry.check_definition_manifest(&draft("absent-engine", serde_json::json!({}), [])),
        Err(ProcessDefinitionRefusal::UnknownEngine {
            engine_kind: ProcessEngineKind::from("absent-engine"),
        })
    );
}

/// The store keeps a descriptor as its canonical bytes: equal descriptors
/// encode equally, and stored bytes decode only to the descriptor of the id
/// they are stored under.
#[test]
fn a_descriptor_round_trips_through_its_canonical_store_bytes() {
    let first = draft(
        "lashlang",
        serde_json::from_str(r#"{"b":[1,-0.0],"a":"x"}"#).expect("parse value"),
        [module("module:1"), env("env:1")],
    );
    let reordered = draft(
        "lashlang",
        serde_json::from_str(r#"{"a":"x","b":[1,0.0]}"#).expect("parse value"),
        [env("env:1"), module("module:1"), env("env:1")],
    );
    assert_eq!(first.to_store_bytes(), reordered.to_store_bytes());
    assert_eq!(
        String::from_utf8(first.to_store_bytes()).expect("utf-8"),
        concat!(
            r#"{"artifacts":[{"artifact_ref":"env:1","store":{"store":"process_env"}},"#,
            r#"{"artifact_ref":"module:1","store":{"store":"lashlang_module"}}],"#,
            r#""engine_kind":"lashlang","value":{"a":"x","b":[1,0.0]}}"#,
        )
    );
    let decoded = ProcessDefinitionDraft::from_store_bytes(&first.id(), &first.to_store_bytes())
        .expect("the stored descriptor decodes");
    assert_eq!(decoded.id(), first.id());
    assert_eq!(decoded.to_store_bytes(), first.to_store_bytes());

    let other = signed_draft();
    assert_eq!(
        ProcessDefinitionDraft::from_store_bytes(&other.id(), &first.to_store_bytes()),
        Err(ProcessDefinitionStoredError::OtherId {
            derived: first.id()
        })
    );
    let spaced = String::from_utf8(first.to_store_bytes())
        .expect("utf-8")
        .replace(':', ": ");
    assert_eq!(
        ProcessDefinitionDraft::from_store_bytes(&first.id(), spaced.as_bytes()),
        Err(ProcessDefinitionStoredError::NotCanonical)
    );
    assert!(matches!(
        ProcessDefinitionDraft::from_store_bytes(&first.id(), b"not json"),
        Err(ProcessDefinitionStoredError::Undecodable(_))
    ));
}

#[test]
fn a_draft_refuses_empty_names_and_unknown_fields() {
    assert_eq!(
        ProcessDefinitionDraft::new("", serde_json::json!({}), []),
        Err(ProcessDefinitionDraftError::EmptyEngineKind)
    );
    assert_eq!(
        ProcessDefinitionDraft::new("lashlang", serde_json::json!({}), [module("")]),
        Err(ProcessDefinitionDraftError::EmptyArtifactRef)
    );
    assert_eq!(
        ProcessDefinitionDraft::new(
            "lashlang",
            serde_json::json!({}),
            [engine_blob("", "blob:1")]
        ),
        Err(ProcessDefinitionDraftError::EmptyArtifactEngineKind)
    );

    let draft = signed_draft();
    let json = serde_json::to_value(&draft).expect("serialize draft");
    assert_eq!(
        json,
        serde_json::json!({
            "engine_kind": SIGNED_ENGINE_KIND,
            "value": {"program": "payout"},
            "artifacts": [{"store": {"store": "lashlang_module"}, "artifact_ref": "module:payout"}],
        })
    );
    assert_eq!(
        serde_json::from_value::<ProcessDefinitionDraft>(json.clone()).expect("deserialize draft"),
        draft
    );
    let mut named = json;
    named["name"] = serde_json::json!("payout");
    assert!(serde_json::from_value::<ProcessDefinitionDraft>(named).is_err());
}
