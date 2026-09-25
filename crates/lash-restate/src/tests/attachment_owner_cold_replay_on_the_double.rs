//! The FIG-546 attachment-owner cold-replay law under the Restate effect
//! engine's in-process face. The law opens two independent controllers over
//! one journal: here the journal is the replayable recording context — the
//! first controller's journaled runs record into it, `start_replay` freezes
//! it, and the reopened controller serves each recorded entry instead of
//! invoking the local executor, the same answer a cold handler replay gives.

use std::sync::Arc;

use super::*;

fn current_epoch_ms_for_test() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

lash_conformance::attachment_owner_cold_replay_tests!({
    // Attachment GC compares blob timestamps against wall-clock
    // `now_epoch_ms`, so the store clock must sit near real time (a little in
    // the past, as the SQLite registration does) rather than at a fixed epoch.
    let clock = Arc::new(lash_core::testing::TestClock::new(
        current_epoch_ms_for_test().saturating_sub(100_000),
    ));
    let stores =
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(Arc::clone(&clock) as Arc<dyn Clock>)
            .await
            .expect("open the attachment-owner store set");
    let context = Arc::new(ReplayableRecordingContext::default());
    let first = Arc::new(RestateRuntimeEffectController::new_for_test(Arc::clone(
        &context,
    ))) as Arc<dyn RuntimeEffectController>;
    let reopen_effect_controller = {
        let context = Arc::clone(&context);
        Arc::new(move || {
            let context = Arc::clone(&context);
            Box::pin(async move {
                context.start_replay();
                Arc::new(RestateRuntimeEffectController::new_for_test(context))
                    as Arc<dyn RuntimeEffectController>
            }) as lash_conformance::ReopenEffectControllerFuture
        }) as lash_conformance::ReopenEffectController
    };
    let advance_clock = {
        let clock = Arc::clone(&clock);
        Arc::new(move |duration_ms| clock.advance(duration_ms)) as Arc<dyn Fn(u64) + Send + Sync>
    };
    (
        stores.clone(),
        lash_conformance::AttachmentOwnerColdReplayBackend {
            session_store_factory: stores.session_store_factory(),
            process_registry: stores.process_registry(),
            attachment_store: stores.attachment_store(),
            first_effect_controller: Some(first),
            reopen_effect_controller,
            clock: clock as Arc<dyn Clock>,
            advance_clock,
        },
    )
});
