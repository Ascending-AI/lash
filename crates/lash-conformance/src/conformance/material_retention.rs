//! Retained tool-material laws (FIG-4889, L12) shared by the SQLite memory,
//! SQLite file and PostgreSQL stores.
//!
//! Every step reads through a freshly reopened store, so a file-backed or
//! PostgreSQL store proves each cut against what it made durable. Each law
//! mints its own Run and source identities and shares no rows with another.

use std::sync::Arc;

use lash_core::store::ToolMaterialStore;
use lash_core::tool_run::{
    MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRef,
    MaterialRetentionError, MaterialRole,
};
use pretty_assertions::assert_eq;

/// A store and a factory that reopens the same durable catalog.
pub struct ReopenableToolMaterialStore {
    pub open: Arc<dyn ToolMaterialStore>,
    pub reopen: Arc<dyn Fn() -> Arc<dyn ToolMaterialStore> + Send + Sync>,
}

/// A fresh logical Run's opener.
fn run(label: &str) -> lash_core::EffectOpener {
    lash_core::EffectOpener::turn(
        lash_core::SessionId::prefixed(
            "material-session-",
            format!("{label}-{}", uuid::Uuid::new_v4().simple()),
        ),
        "material-turn",
    )
}

fn run_owner(opener: &lash_core::EffectOpener) -> MaterialOwner {
    MaterialOwner::Run {
        opener: opener.clone(),
    }
}

fn payload(owner: &MaterialOwner, role: MaterialRole, text: &str) -> MaterialPayload {
    MaterialPayload::new(owner.clone(), role, None, text.repeat(512))
}

#[expect(clippy::expect_used, reason = "law fixture encodes canonical payloads")]
fn bundle(payloads: Vec<MaterialPayload>) -> MaterialBundle {
    MaterialBundle::of(payloads)
        .expect("encode bundle")
        .expect("a non-empty bundle")
}

async fn read(
    store: &Arc<dyn ToolMaterialStore>,
    holder: &MaterialHolder,
    reference: &MaterialRef,
) -> Result<String, MaterialRetentionError> {
    store
        .read_material(holder, reference, &reference.owner, &[])
        .await
        .map(|payload| payload.text)
}

fn refusal_code(result: Result<impl std::fmt::Debug, MaterialRetentionError>) -> &'static str {
    match result {
        Err(MaterialRetentionError::Refused(refusal)) => refusal.code(),
        Err(MaterialRetentionError::HolderEnded { .. }) => "holder_ended",
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

/// L12 and K4: a Deferred source retains its result under its own lease
/// before its seal publishes it; the consumer reads it as the source's
/// owner and nobody else's; the read checks the codec revision; and a
/// reference to material that was never retained refuses typed.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates every store step"
)]
pub async fn source_material_reads_refuse_typed_without_a_fresh_body<F>(make: F)
where
    F: Fn() -> ReopenableToolMaterialStore,
{
    let fixture = make();
    let opener = run("source-consumer");
    let consumer = run_owner(&opener);
    let source_session = format!("material-source-{}", uuid::Uuid::new_v4().simple());
    let source = lash_core::AwaitEventKey {
        scope: lash_core::ExecutionScope::turn(
            lash_core::SessionId::fixture(source_session),
            "material-turn",
        ),
        wait: lash_core::AwaitEventWaitIdentity::SessionCommandCancelSignal,
        key_id: "material-key".into(),
        signature: "material-signature".into(),
    };
    let source_owner = MaterialOwner::Source {
        source: source.clone(),
    };
    let holder = MaterialHolder::Source {
        source: source.clone(),
    };
    let codec =
        crate::plugin::PluginRevision::new("material-codec", crate::plugin::BehaviorRevision::ONE);
    let result = MaterialPayload::new(
        source_owner.clone(),
        MaterialRole::AttemptOutput,
        Some(codec.clone()),
        "deferred-result ".repeat(512),
    );
    let retained = fixture
        .open
        .retain_material(&holder, &bundle(vec![result.clone()]))
        .await
        .expect("the source retains before sealing");
    let reference = &retained.references[0];
    let store = (fixture.reopen)();
    assert_eq!(
        store
            .read_material(
                &holder,
                reference,
                &source_owner,
                std::slice::from_ref(&codec)
            )
            .await
            .expect("the consumer reads the sealed result")
            .text,
        result.text
    );
    let wrong_owner = store
        .read_material(&holder, reference, &consumer, std::slice::from_ref(&codec))
        .await;
    assert_eq!(refusal_code(wrong_owner), "material_wrong_owner");
    let unavailable = store
        .read_material(&holder, reference, &source_owner, &[])
        .await;
    assert_eq!(refusal_code(unavailable), "material_revision_mismatch");
    let unretained = bundle(vec![payload(
        &consumer,
        MaterialRole::AttemptOutput,
        "never ",
    )]);
    assert_eq!(
        refusal_code(read(&store, &holder, &unretained.references()[0]).await),
        "material_missing"
    );
    store
        .release_material(&holder)
        .await
        .expect("the consumer releases the source after consuming");
    assert_eq!(
        refusal_code(
            (fixture.reopen)()
                .read_material(
                    &holder,
                    reference,
                    &source_owner,
                    std::slice::from_ref(&codec)
                )
                .await
        ),
        "material_retired"
    );
}
