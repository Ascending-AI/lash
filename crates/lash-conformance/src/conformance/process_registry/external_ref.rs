use super::*;
use pretty_assertions::assert_eq;

/// FIG-2964: the external reference is written compare-and-set, keyed by the
/// execution segment it was minted for — never last-write-wins.
///
/// A recovery sweep and a live handover can both hold a reference for the same
/// row. Last-write-wins would let the loser's stale segment-0 reference land on
/// top of the segment the process actually advanced to, and every later reader
/// — cancel, observation, terminal settlement — would address the wrong
/// workflow. So a write lands only when nothing is recorded yet or when it
/// carries a strictly later ordinal; an equal or earlier ordinal on the same
/// backend is an idempotent no-op, which is what lets a resubmitting sweep
/// coalesce without rewriting the row it coalesced onto.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn external_ref_is_written_compare_and_set_by_segment_ordinal(
    registry: Arc<dyn ProcessRegistry>,
) {
    let process_id = ProcessId::from("external-ref-compare-and-set");
    registry
        .register_process(registration("external-ref-compare-and-set"))
        .await
        .expect("register process");
    let reference = |ordinal: Option<u64>, id: &str| crate::ProcessExternalRef {
        backend: "restate".to_string(),
        id: id.to_string(),
        metadata: None,
        segment_ordinal: ordinal,
    };
    let recorded = |record: &crate::ProcessRecord| {
        let external = record
            .external_ref
            .as_ref()
            .expect("external reference recorded");
        (external.id.clone(), external.segment_ordinal)
    };

    // Unset: this writer names the owner.
    let record = registry
        .set_external_ref(&process_id, reference(Some(1), "workflow#1"))
        .await
        .expect("first reference lands");
    assert_eq!(recorded(&record), ("workflow#1".to_string(), Some(1)));

    // Earlier segment: a sweep that lost the race must not rewind the row.
    let record = registry
        .set_external_ref(&process_id, reference(Some(0), "workflow"))
        .await
        .expect("an earlier-segment write is an idempotent no-op, not a refusal");
    assert_eq!(
        recorded(&record),
        ("workflow#1".to_string(), Some(1)),
        "an earlier segment must never displace the recorded owner"
    );

    // A missing ordinal reads as segment 0, so a predecessor's reference cannot
    // silently win by omitting the field.
    let record = registry
        .set_external_ref(&process_id, reference(None, "workflow-legacy"))
        .await
        .expect("an ordinal-less write is an idempotent no-op");
    assert_eq!(recorded(&record), ("workflow#1".to_string(), Some(1)));

    // Same segment: idempotent, so a resubmitting sweep coalesces cleanly.
    let record = registry
        .set_external_ref(
            &process_id,
            reference(Some(1), "workflow#1-other-invocation"),
        )
        .await
        .expect("a same-segment write is an idempotent no-op");
    assert_eq!(recorded(&record), ("workflow#1".to_string(), Some(1)));

    // Strictly later segment: the handover's reference displaces the old one.
    let record = registry
        .set_external_ref(&process_id, reference(Some(2), "workflow#2"))
        .await
        .expect("a later-segment write displaces the recorded owner");
    assert_eq!(recorded(&record), ("workflow#2".to_string(), Some(2)));

    // A competing backend is a refusal at every ordinal: a row has exactly one
    // durable owner substrate, and a later segment never licenses a change of
    // substrate.
    assert_session_refusal(
        registry
            .set_external_ref(
                &process_id,
                crate::ProcessExternalRef {
                    backend: "other-backend".to_string(),
                    id: "workflow#9".to_string(),
                    metadata: None,
                    segment_ordinal: Some(9),
                },
            )
            .await,
        "process `external-ref-compare-and-set` external ref conflict: existing restate / workflow#2, requested other-backend / workflow#9",
    );
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read process")
        .expect("row stands");
    assert_eq!(
        recorded(&record),
        ("workflow#2".to_string(), Some(2)),
        "a refused cross-backend write must leave the recorded owner untouched"
    );
}
