//! Laws of the trace causality vocabulary.

use super::*;
use serde_json::json;

const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const UNSAMPLED: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";

fn carrier(span: u64) -> TraceCarrier {
    TraceCarrier::new(
        W3cTraceId::from_bytes([7; 16]).unwrap(),
        W3cSpanId::from_bytes(span.to_be_bytes()).unwrap(),
        W3cTraceFlags::from_byte(W3cTraceFlags::SAMPLED),
        W3cTraceState::default(),
    )
}

fn run_scope() -> TraceScopeId {
    TraceScopeId::admission(TraceScopeOwner::Turn {
        session_id: SessionId::from("session"),
        turn_id: TurnId::from("run"),
    })
}

/// P1: a valid context survives the codec, sampled or not; a malformed parent
/// is absent; a bad tracestate does not cost a valid parent; the limits and
/// zero ids are enforced; no cause and a parent cause round-trip.
#[test]
fn carrier_codec_preserves_valid_context_and_rejects_invalid_fields() {
    let sampled =
        TraceCarrier::parse_w3c(PARENT, Some("rojo=00f067aa0ba902b7, congo=t61rcWkgMzE")).unwrap();
    assert_eq!(sampled.traceparent(), PARENT);
    assert!(sampled.flags().is_sampled());
    assert_eq!(
        sampled.tracestate().as_str(),
        "rojo=00f067aa0ba902b7,congo=t61rcWkgMzE"
    );
    assert_eq!(
        sampled.tracestate().members().collect::<Vec<_>>(),
        [("rojo", "00f067aa0ba902b7"), ("congo", "t61rcWkgMzE")]
    );
    assert_eq!(
        serde_json::to_value(&sampled).unwrap(),
        json!({
            "traceparent": PARENT,
            "tracestate": "rojo=00f067aa0ba902b7,congo=t61rcWkgMzE",
        })
    );

    // An unsampled context is a context, not an absence.
    let unsampled = TraceCarrier::parse_w3c(UNSAMPLED, None).unwrap();
    assert!(!unsampled.flags().is_sampled());
    assert_eq!(
        serde_json::to_value(&unsampled).unwrap(),
        json!({ "traceparent": UNSAMPLED })
    );
    for context in [&sampled, &unsampled] {
        let stored = serde_json::to_string(context).unwrap();
        assert_eq!(
            &serde_json::from_str::<TraceCarrier>(&stored).unwrap(),
            context
        );
    }

    // The explicit parser names what is wrong.
    let refused = |traceparent: &str| TraceCarrier::parse_w3c(traceparent, None).unwrap_err();
    assert_eq!(
        refused("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
        InvalidTraceCarrier::ZeroTraceId
    );
    assert_eq!(
        refused("00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01"),
        InvalidTraceCarrier::ZeroSpanId
    );
    assert_eq!(
        refused("00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01"),
        InvalidTraceCarrier::TraceparentEncoding
    );
    assert_eq!(
        refused("ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        InvalidTraceCarrier::TraceparentVersion
    );
    assert_eq!(
        refused("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7"),
        InvalidTraceCarrier::TraceparentShape
    );
    assert_eq!(
        refused(&format!("{PARENT}-extra")),
        InvalidTraceCarrier::TraceparentShape
    );
    assert_eq!(refused("é"), InvalidTraceCarrier::TraceparentShape);
    // A later version may carry more fields; its first four are read.
    let later = "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-more";
    assert_eq!(
        TraceCarrier::parse_w3c(later, None).unwrap().traceparent(),
        PARENT
    );

    // Tracestate limits.
    let members = |count: usize| {
        (0..count)
            .map(|index| format!("k{index}=v"))
            .collect::<Vec<_>>()
            .join(",")
    };
    assert!(W3cTraceState::parse(&members(TRACESTATE_MEMBER_LIMIT)).is_ok());
    assert_eq!(
        W3cTraceState::parse(&members(TRACESTATE_MEMBER_LIMIT + 1)).unwrap_err(),
        InvalidTraceCarrier::TracestateMembers {
            members: TRACESTATE_MEMBER_LIMIT + 1,
            limit: TRACESTATE_MEMBER_LIMIT
        }
    );
    let long = format!("a={},b={}", "x".repeat(255), "y".repeat(253));
    assert_eq!(long.len(), TRACESTATE_CHAR_LIMIT + 1);
    assert_eq!(
        W3cTraceState::parse(&long).unwrap_err(),
        InvalidTraceCarrier::TracestateLength {
            chars: TRACESTATE_CHAR_LIMIT + 1,
            limit: TRACESTATE_CHAR_LIMIT
        }
    );
    assert_eq!(
        W3cTraceState::parse("a=1,a=2").unwrap_err(),
        InvalidTraceCarrier::TracestateDuplicateKey { index: 1 }
    );
    for bad in ["Upper=1", "a", "a=", "a=b=c", "a=\u{e9}", "@sys=1"] {
        assert_eq!(
            W3cTraceState::parse(bad).unwrap_err(),
            InvalidTraceCarrier::TracestateMember { index: 0 },
            "{bad:?}"
        );
    }
    assert!(W3cTraceState::parse("1tenant@sys=v, ,simple/key-*_=v v").is_ok());

    // Transport extraction is tolerant where the explicit parser is strict.
    assert_eq!(TraceCarrier::extract_w3c(None, Some("a=1")), None);
    assert_eq!(TraceCarrier::extract_w3c(Some("garbage"), None), None);
    assert_eq!(
        TraceCarrier::extract_w3c(
            Some("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
            None
        ),
        None
    );
    let survivor = TraceCarrier::extract_w3c(Some(PARENT), Some("a=1,a=2")).unwrap();
    assert_eq!(survivor.traceparent(), PARENT);
    assert!(survivor.tracestate().is_empty());
    assert!(TraceCarrier::parse_w3c(PARENT, Some("a=1,a=2")).is_err());

    // Invalid retained data is a decode error, not a missing cause.
    for stored in [
        json!({ "traceparent": "garbage" }),
        json!({ "traceparent": PARENT, "tracestate": "a=1,a=2" }),
        json!({ "traceparent": PARENT, "baggage": "k=v" }),
    ] {
        assert!(serde_json::from_value::<TraceCarrier>(stored).is_err());
    }

    // No cause and a parent keep their relation through storage.
    for cause in [TraceCause::None, TraceCause::Parent(sampled.clone())] {
        let stored = serde_json::to_string(&cause).unwrap();
        assert_eq!(serde_json::from_str::<TraceCause>(&stored).unwrap(), cause);
    }
    assert_eq!(
        serde_json::to_value(TraceCause::None).unwrap(),
        json!({ "kind": "none" })
    );
    assert_eq!(
        serde_json::to_value(TraceCause::Parent(unsampled)).unwrap(),
        json!({ "kind": "parent", "traceparent": UNSAMPLED })
    );
}

#[test]
fn links_keep_the_first_sixty_four_distinct_spans_in_admission_order() {
    let mut links = TraceLinks::new();
    for span in 1..=70_u64 {
        links.push(carrier(span));
        links.push(carrier(span));
    }
    assert_eq!(links.contexts().len(), TRACE_LINK_LIMIT);
    assert_eq!(links.contexts()[0], carrier(1));
    assert_eq!(links.contexts()[63], carrier(64));
    // 65..=70 arrive twice each past the limit: none is retained to dedup against.
    assert_eq!(links.omitted(), 12);

    let stored = serde_json::to_value(&links).unwrap();
    assert_eq!(serde_json::from_value::<TraceLinks>(stored).unwrap(), links);

    let over: Vec<_> = (1..=65_u64).map(carrier).collect();
    assert!(serde_json::from_value::<TraceLinks>(json!({ "contexts": over })).is_err());
    assert!(
        serde_json::from_value::<TraceLinks>(json!({ "contexts": [carrier(1), carrier(1)] }))
            .is_err()
    );
}

#[test]
fn a_retained_scope_yields_no_permit_and_an_inserted_one_does() {
    let parent = TraceCause::Parent(carrier(3)).as_parent();
    let scope = DurableTraceScope::admit(
        run_scope(),
        parent.as_ref(),
        [carrier(4)].into_iter().collect(),
        1_700_000_000_000,
    );
    let span = derive_span(
        &carrier(3),
        &SpanKey::Scope {
            scope: &run_scope(),
        },
    );
    assert_eq!(scope.span.as_ref(), Some(&span));
    assert_eq!(
        scope.as_parent().map(|parent| parent.carrier().clone()),
        Some(span.clone())
    );

    let stored = serde_json::to_value(&scope).unwrap();
    assert_eq!(
        stored,
        json!({
            "scope": { "owner": { "kind": "turn", "session_id": "session", "turn_id": "run" } },
            "cause": { "kind": "parent", "traceparent": carrier(3).traceparent() },
            "links": { "contexts": [{ "traceparent": carrier(4).traceparent() }] },
            "span": { "traceparent": span.traceparent() },
            "started_at_ms": 1_700_000_000_000_u64,
        })
    );
    assert_eq!(
        serde_json::from_value::<DurableTraceScope>(stored).unwrap(),
        scope
    );

    let inserted = TraceScopeAdmission::Inserted(scope.clone());
    let existing = TraceScopeAdmission::Existing(scope.clone());
    assert_eq!(
        inserted.permit().unwrap().source(),
        &EmissionSource::NewTransition
    );
    assert!(existing.permit().is_none());
    assert_eq!(existing.scope(), &scope);
}

/// No admission makes a root: a scope admitted with no parent is
/// unobserved, links nothing and has no span, and its children have no
/// parent either.
#[test]
fn a_scope_admitted_without_a_parent_is_unobserved() {
    let scope = DurableTraceScope::admit(run_scope(), None, [carrier(4)].into_iter().collect(), 1);
    assert_eq!(scope.cause, TraceCause::None);
    assert!(scope.links.is_empty());
    assert_eq!(scope.span, None);
    assert_eq!(scope.as_parent(), None);
    assert!(scope.offer().is_empty());
}

/// A container's span is a pure function of its parent and its durable
/// key: every owner derives the same id, another key or parent another,
/// and it keeps the parent's trace id, flags and tracestate.
#[test]
fn a_container_span_derives_from_its_parent_and_key_alone() {
    let mut parent = carrier(3);
    parent = TraceCarrier::new(
        parent.trace_id(),
        parent.span_id(),
        W3cTraceFlags::from_byte(0),
        W3cTraceState::parse("vendor=x").unwrap(),
    );
    let scope = run_scope();
    let key = SpanKey::Scope { scope: &scope };
    let span = derive_span(&parent, &key);
    assert_eq!(span, derive_span(&parent, &key));
    assert_eq!(span.trace_id(), parent.trace_id());
    assert_eq!(span.flags(), parent.flags());
    assert_eq!(span.tracestate(), parent.tracestate());
    assert_ne!(span.span_id(), parent.span_id());
    let other = run_scope().at_boundary(1);
    assert_ne!(
        span.span_id(),
        derive_span(&parent, &SpanKey::Scope { scope: &other }).span_id()
    );
    assert_ne!(
        span.span_id(),
        derive_span(&carrier(4), &key).span_id(),
        "another parent derives another span"
    );
    assert_ne!(
        derive_span(&parent, &SpanKey::Cell { graph_key: "a" }).span_id(),
        derive_span(&parent, &SpanKey::Cell { graph_key: "b" }).span_id()
    );
}

/// With no adapter, a producer joins its caller in parent mode and roots a
/// new sampled trace in link mode or without a caller.
#[test]
fn lash_identity_joins_or_roots_a_producer_by_its_link_mode() {
    let caller = TraceCarrier::new(
        carrier(3).trace_id(),
        carrier(3).span_id(),
        W3cTraceFlags::from_byte(0),
        W3cTraceState::parse("vendor=x").unwrap(),
    );
    let spec = |caller: Option<TraceCarrier>, link_mode| ProducerSpec {
        kind: ProducerKind::Send,
        caller,
        link_mode,
        ids: crate::TraceIds::default(),
    };
    let child = LashIdentity.open_producer(&spec(Some(caller.clone()), TraceLinkMode::Parent));
    assert_eq!(child.trace_id(), caller.trace_id());
    assert_eq!(child.flags(), caller.flags());
    assert_eq!(child.tracestate(), caller.tracestate());
    assert_ne!(child.span_id(), caller.span_id());
    for root in [
        LashIdentity.open_producer(&spec(Some(caller.clone()), TraceLinkMode::Link)),
        LashIdentity.open_producer(&spec(None, TraceLinkMode::Parent)),
    ] {
        assert_ne!(root.trace_id(), caller.trace_id());
        assert!(root.flags().is_sampled());
        assert_eq!(root.tracestate(), &W3cTraceState::default());
    }
    assert_ne!(LashIdentity.leaf_span_id(), LashIdentity.leaf_span_id());
}

#[test]
fn record_identity_is_stable_per_fact_and_distinct_per_attempt() {
    let terminal = |ordinal| TraceRecordIdentity::Transition {
        scope: run_scope(),
        transition: TraceTransitionKind::Terminal,
        ordinal,
    };
    let live = |attempt: &str| TraceRecordIdentity::Live {
        scope: run_scope(),
        attempt: TraceAttemptId::new(attempt),
        ordinal: 0,
    };
    let id = terminal(0).record_id().unwrap();
    assert_eq!(id.len(), 32);
    assert_eq!(id, terminal(0).record_id().unwrap());
    assert_ne!(id, terminal(1).record_id().unwrap());
    assert_ne!(
        id,
        TraceRecordIdentity::Transition {
            scope: run_scope().at_boundary(1),
            transition: TraceTransitionKind::Terminal,
            ordinal: 0,
        }
        .record_id()
        .unwrap()
    );
    assert_eq!(
        live("a").record_id().unwrap(),
        live("a").record_id().unwrap()
    );
    assert_ne!(
        live("a").record_id().unwrap(),
        live("b").record_id().unwrap()
    );

    let at = crate::TraceTimestamp::from_unix_ms(1_700_000_000_000);
    let event = || crate::TraceEvent::Custom {
        name: "test.event".to_string(),
        payload: json!({}),
    };
    let first =
        crate::TraceRecord::identified(&terminal(0), crate::TraceContext::default(), event(), at)
            .unwrap();
    let again =
        crate::TraceRecord::identified(&terminal(0), crate::TraceContext::default(), event(), at)
            .unwrap();
    assert_eq!(first, again);
    assert_eq!(first.id, id);
}

/// The cause of a scope that admits several rows: the head row's producer
/// is its parent, every other row's producer is a link in admission order,
/// and a head with no cause admits the scope unobserved.
#[test]
fn an_admitting_scope_parents_its_head_and_links_the_rest() {
    let caused = |span| TraceCause::Parent(carrier(span));
    let (parent, links) = TraceCause::of_admitted([]);
    assert_eq!(parent, None);
    assert!(links.is_empty());
    let (parent, links) = TraceCause::of_admitted([&TraceCause::None, &caused(1)]);
    assert_eq!(parent, None, "a head with no cause is unobserved");
    assert!(links.is_empty());
    let (parent, links) = TraceCause::of_admitted([&caused(1)]);
    assert_eq!(
        parent.map(|parent| parent.carrier().clone()),
        Some(carrier(1))
    );
    assert!(links.is_empty());

    let (parent, links) = TraceCause::of_admitted([
        &caused(9),
        &caused(1),
        &TraceCause::None,
        &caused(9),
        &caused(2),
        &caused(1),
    ]);
    assert_eq!(
        parent.map(|parent| parent.carrier().clone()),
        Some(carrier(9))
    );
    assert_eq!(
        links
            .contexts()
            .iter()
            .map(TraceCarrier::span_id)
            .collect::<Vec<_>>(),
        [carrier(1).span_id(), carrier(2).span_id()],
        "each other producer is linked once, in the order its row was admitted; the parent is never a link"
    );
}

/// TRACE-CARRIER-POLICY: host-selected cuts stay inside the W3C and stored-link ceilings.
#[test]
fn host_trace_state_and_link_limits_are_honored() {
    let limits = TraceStateLimits::new(1, 512, 256, 241, 14, 256).unwrap();
    assert!(matches!(
        W3cTraceState::parse_with_limits("a=1,b=2", limits),
        Err(InvalidTraceCarrier::TracestateMembers { limit: 1, .. })
    ));
    let mut links = TraceLinks::with_limit(1).unwrap();
    links.push(carrier(1));
    links.push(carrier(2));
    assert_eq!(links.contexts().len(), 1);
    assert_eq!(links.omitted(), 1);
    assert_eq!(
        serde_json::from_value::<TraceLinks>(serde_json::to_value(&links).unwrap()).unwrap(),
        links
    );
}
