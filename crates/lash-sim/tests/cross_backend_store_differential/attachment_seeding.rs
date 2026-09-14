//! Manifest rows the differential compares across backends.
//!
//! Adoption is gated on recorded upload evidence, so seeding a comparable row
//! is no longer a single write: the attempt is minted under the backend's
//! write fence and stamped only when the put succeeded. Both the stamped and
//! the unstamped shape must read back identically on every backend.

use lash_core::store::StoreError;
use lash_core::{AttachmentIntent, AttachmentOwner, AttachmentWriteFence, SessionId};

use super::ConformancePersistence;

/// Seed the two manifest rows the differential observes.
///
/// The turn-owned row carries positive upload evidence; the process-owned row
/// is deliberately left unstamped, because an attempt that never completed
/// must also read back identically on every backend. The process owner
/// identity is `(process_id, incarnation)`, so this row is what proves both
/// backends persist and return the incarnation rather than the turn shape that
/// leaves the column NULL. The fixture wires no process registry, so the row
/// stays an immortal root everywhere.
pub(crate) async fn seed_differential_attachment_rows(
    store: &dyn ConformancePersistence,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let operation =
        lash_core::store::OperationId::turn(session_id, "attachment-adoption", "differential")
            .storage_key()?;
    let turn_owned = AttachmentIntent {
        attachment_id: super::differential_attachment_id(),
        session_id: session_id.clone(),
        canonical_uri: "lash-attachment://blake3/differential-attachment".to_string(),
        intent_at_epoch_ms: 1_000,
        owner: Some(AttachmentOwner::Turn { id: operation }),
    };
    let AttachmentWriteFence::Granted(turn_permit) =
        store.begin_attachment_write(turn_owned.clone()).await?
    else {
        panic!("the differential digest must grant its writer");
    };
    store
        .complete_attachment_write(&turn_owned, turn_permit)
        .await?;

    let process_owned = AttachmentIntent {
        attachment_id: super::differential_process_attachment_id(),
        session_id: session_id.clone(),
        canonical_uri: "lash-attachment://blake3/differential-process-attachment".to_string(),
        intent_at_epoch_ms: 1_000,
        owner: Some(AttachmentOwner::Process {
            id: super::DIFFERENTIAL_PROCESS_OWNER_ID.to_string(),
            incarnation: lash_core::ProcessIncarnation::from_registration_sequence(
                super::DIFFERENTIAL_PROCESS_OWNER_INCARNATION,
            ),
        }),
    };
    let AttachmentWriteFence::Granted(_) = store.begin_attachment_write(process_owned).await?
    else {
        panic!("the process-owned digest must grant its writer");
    };
    Ok(())
}
