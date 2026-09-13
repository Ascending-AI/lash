//! Byte-identity witnesses and vocabulary-completeness guards for
//! [`super`]. Exempt from the raw-literal gate by the `_tests.rs` path rule:
//! the previous spellings must live somewhere to be proved unchanged.

use super::*;

#[test]
fn live_and_retired_statuses_partition_the_vocabulary() {
    for status in ProcessStatus::ALL {
        assert_ne!(
            status.is_live(),
            status.is_retired(),
            "{status:?} must be exactly one of live or retired: the retention \
             predicate is spelled as the complement of the live set"
        );
    }
}

/// Byte-identity witnesses for the literals these fragments replaced.
#[test]
fn generated_fragments_match_the_previous_literals() {
    assert_eq!(live_process_statuses_sql(), "'running', 'waiting'");
    let retired = ProcessStatus::ALL
        .iter()
        .copied()
        .filter(ProcessStatus::is_retired)
        .collect::<Vec<_>>();
    assert_eq!(
        process_status_sql_literal_list(&retired),
        "'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed'"
    );
    assert_eq!(
        live_process_status_predicate_sql("status"),
        "status IN ('running', 'waiting')"
    );
    assert_eq!(
        live_process_status_predicate_sql("p.status"),
        "p.status IN ('running', 'waiting')"
    );
    assert_eq!(
        live_process_status_predicate_sql("processes.status"),
        "processes.status IN ('running', 'waiting')"
    );
    assert_eq!(
        retired_process_status_predicate_sql("status"),
        "status NOT IN ('running', 'waiting')"
    );
    assert_eq!(
        retired_process_status_predicate_sql("p.status"),
        "p.status NOT IN ('running', 'waiting')"
    );
    assert_eq!(
        undelivered_wake_delivery_states_sql(),
        "'pending', 'enqueuing'"
    );
    assert_eq!(
        undelivered_wake_delivery_state_predicate_sql("state"),
        "state IN ('pending', 'enqueuing')"
    );
    assert_eq!(
        undelivered_wake_delivery_state_predicate_sql("delivery.state"),
        "delivery.state IN ('pending', 'enqueuing')"
    );
    assert_eq!(
        wake_delivery_state_sql_literal(WakeDeliveryState::Pending),
        "'pending'"
    );
    assert_eq!(
        wake_delivery_state_sql_literal(WakeDeliveryState::Enqueuing),
        "'enqueuing'"
    );
    assert_eq!(
        wake_delivery_state_sql_literal(WakeDeliveryState::Enqueued),
        "'enqueued'"
    );
    assert_eq!(
        wake_delivery_state_sql_literal(WakeDeliveryState::Discarded),
        "'discarded'"
    );
    assert_eq!(
        process_status_sql_literal(ProcessStatus::CallerDeparted),
        "'caller_departed'"
    );
}
