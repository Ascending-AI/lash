//! L20 and L21(c) (FIG-6300): every tracing knob is a sealed policy with a
//! documented default and range, refused with a typed field, value and
//! bound; every legal endpoint builds, and the core reports it. No kind is
//! off while a kind it can parent is on.

use super::*;

use lash::tracing::{
    ContentLimits, FlightRecorderSettings, PolicyOperand, PolicyRelation, PolicyValue, TraceDetail,
    TraceDetailPolicy, TraceExportPolicy, TraceLimits, TracePolicyError, TracePolicyField as F,
    TracePolicySelection, TraceSpanKind as K, TraceStateLimits, attribute_reserve,
};
use std::time::Duration;

/// `field = value` over the standard values of its policy, with
/// `companions` set first: what the field's policy answers, and the value it
/// reports once built. A field the dispatch does not know fails the law, so
/// a new field cannot skip it.
fn attempt(
    field: F,
    value: u64,
    companions: &[(F, u64)],
) -> std::result::Result<u64, TracePolicyError> {
    match field {
        F::MaxPartBytes | F::MaxAttributeBytes | F::MaxProviderBodyBytes => {
            let values = [
                F::MaxPartBytes,
                F::MaxAttributeBytes,
                F::MaxProviderBodyBytes,
            ]
            .map(|row| {
                if row == field {
                    value
                } else {
                    companions
                        .iter()
                        .find_map(|(f, v)| (*f == row).then_some(*v))
                        .unwrap_or_else(|| default(row))
                }
            });
            let limits = ContentLimits::new(values[0], values[1], values[2])?;
            Ok(match field {
                F::MaxPartBytes => limits.max_part_bytes(),
                F::MaxAttributeBytes => limits.max_attribute_bytes(),
                _ => limits.max_provider_body_bytes(),
            })
        }

        F::TracestateMembers
        | F::TracestateChars
        | F::TracestateKeyBytes
        | F::TracestateTenantBytes
        | F::TracestateSystemBytes
        | F::TracestateValueBytes => {
            let rows = [
                F::TracestateMembers,
                F::TracestateChars,
                F::TracestateKeyBytes,
                F::TracestateTenantBytes,
                F::TracestateSystemBytes,
                F::TracestateValueBytes,
            ]
            .map(|row| if row == field { value } else { default(row) });
            let limits =
                TraceStateLimits::new(rows[0], rows[1], rows[2], rows[3], rows[4], rows[5])?;
            let reported = match field {
                F::TracestateMembers => limits.members(),
                F::TracestateChars => limits.chars(),
                F::TracestateKeyBytes => limits.key_bytes(),
                F::TracestateTenantBytes => limits.tenant_bytes(),
                F::TracestateSystemBytes => limits.system_bytes(),
                _ => limits.value_bytes(),
            };
            Ok(u64::try_from(reported).expect("a tracestate limit"))
        }
        F::RecorderCapacity => FlightRecorderSettings::new(
            value,
            Some(Duration::from_millis(default(F::RecorderSlowThreshold))),
            Duration::from_millis(default(F::RecorderMinimumInterval)),
            true,
        )
        .map(|settings| u64::try_from(settings.capacity().get()).expect("a capacity")),
        F::RecorderSlowThreshold => FlightRecorderSettings::new(
            default(F::RecorderCapacity),
            Some(Duration::from_millis(value)),
            Duration::from_millis(default(F::RecorderMinimumInterval)),
            true,
        )
        .map(|settings| {
            settings.slow_threshold().map_or(0, |threshold| {
                u64::try_from(threshold.as_millis()).expect("a threshold")
            })
        }),
        F::RecorderMinimumInterval => FlightRecorderSettings::new(
            default(F::RecorderCapacity),
            Some(Duration::from_millis(default(F::RecorderSlowThreshold))),
            Duration::from_millis(value),
            true,
        )
        .map(|settings| {
            u64::try_from(settings.minimum_interval().as_millis()).expect("an interval")
        }),
        _ => {
            let limits = limits(
                companions
                    .iter()
                    .copied()
                    .chain(std::iter::once((field, value))),
            )?;
            Ok(limits
                .values()
                .find_map(|(row, reported)| (row == field).then_some(reported))
                .unwrap_or_else(|| panic!("{field} is no trace limit")))
        }
    }
}

/// [`TraceLimits`] with `rows` set over the standard values.
fn limits(
    rows: impl IntoIterator<Item = (F, u64)>,
) -> std::result::Result<TraceLimits, TracePolicyError> {
    let mut builder = TraceLimits::builder();
    let mut hold = (
        default(F::PublicationHoldRecords),
        default(F::PublicationHoldBytes),
    );
    for (row, value) in rows {
        builder = match row {
            F::HeldObservations => builder.held_observations(value),
            F::Links => builder.links(value),
            F::ExtensionSpans => builder.extension_spans(value),
            F::AttributesPerSpan => builder.attributes_per_span(value),
            F::EventsPerSpan => builder.events_per_span(value),
            F::AttributeValueBytes => builder.attribute_value_bytes(value),
            F::ExtensionBytes => builder.extension_bytes(value),
            F::ToolSpanAttributes => builder.tool_span_attributes(value),
            F::NameBytes => builder.name_bytes(value),
            F::AttributeKeyBytes => builder.attribute_key_bytes(value),
            F::SpanContributionBytes => builder.span_contribution_bytes(value),
            F::References => builder.references(value),
            F::ReferenceBytes => builder.reference_bytes(value),
            F::PublicationHoldRecords => {
                hold.0 = value;
                builder
            }
            F::PublicationHoldBytes => {
                hold.1 = value;
                builder
            }
            other => panic!("the law has no dispatch for {other}"),
        };
    }
    builder.publication_hold(hold.0, hold.1).build()
}

fn default(field: F) -> u64 {
    field.bounds().expect("a numeric field").default
}

/// The kind with the largest reserve among `kinds`, the first on a tie.
fn widest(kinds: &[K]) -> (K, u64) {
    kinds
        .iter()
        .map(|kind| (*kind, u64::from(attribute_reserve(*kind))))
        .fold(None::<(K, u64)>, |best, candidate| match best {
            Some(best) if best.1 >= candidate.1 => Some(best),
            _ => Some(candidate),
        })
        .expect("a kind")
}

const CONTENT_KINDS: [K; 4] = [K::Model, K::Tool, K::Cell, K::Extension];

/// What a legal endpoint of `field` needs of the other rows under the
/// documented relations (§6.7.1): the smallest companion values, and the
/// detail policy the core needs for it.
fn companions(field: F, value: u64) -> (Vec<(F, u64)>, TraceDetailPolicy) {
    if matches!(field, F::MaxPartBytes | F::MaxProviderBodyBytes)
        && value > default(F::MaxAttributeBytes)
    {
        return (
            vec![(F::MaxAttributeBytes, value)],
            TraceDetailPolicy::standard(),
        );
    }
    if field == F::MaxAttributeBytes {
        return (
            vec![(F::MaxPartBytes, 1024), (F::MaxProviderBodyBytes, 0)],
            TraceDetailPolicy::standard(),
        );
    }

    let mut rows = Vec::new();
    let mut detail = TraceDetailPolicy::standard();
    match field {
        // At the largest reserve no contribution slot is left for the tool
        // span's annotations or a content record's references.
        F::AttributesPerSpan if value < default(F::AttributesPerSpan) => {
            rows.push((F::ToolSpanAttributes, 0));
            rows.push((F::References, 0));
        }
        F::SpanContributionBytes if value < default(F::ReferenceBytes) => {
            rows.push((F::ReferenceBytes, 0));
        }
        F::ReferenceBytes if value > default(F::SpanContributionBytes) => {
            rows.push((F::SpanContributionBytes, value));
        }
        F::ExtensionSpans if value == 0 => {
            detail = TraceDetailPolicy::builder()
                .set(K::Model, TraceDetail::Off)
                .set(K::ModelAttempt, TraceDetail::Off)
                .build()
                .expect("model calls off with their attempts");
        }
        _ => {}
    }
    (rows, detail)
}

/// A core under `limits`, `content` and `detail`, and the policy it reports.
async fn reported(
    limits: TraceLimits,
    content: ContentLimits,
    detail: TraceDetailPolicy,
) -> lash::Result<lash::tracing::TracePolicySnapshot> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_sessions(false)
    .trace_limits(limits)
    .content_limits(content)
    .trace_detail(detail)
    .build(lash_facade_tests::runtime_lease_owner())?;
    let policy = core.trace_policy().clone();
    core.shutdown().await?;
    Ok(policy)
}

/// L20 over the tracing policies: below `min`, above `max`, both endpoints,
/// every documented relation's smallest violation, and the defaults.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_tracing_knob_is_refused_typed_out_of_range_and_builds_at_its_endpoints()
-> lash::Result<()> {
    let standard = reported(
        TraceLimits::standard(),
        ContentLimits::standard(),
        TraceDetailPolicy::standard(),
    )
    .await?;
    assert_eq!(
        standard,
        lash::tracing::TracePolicySnapshot::standard(),
        "defaults build"
    );
    let mut unsigned_floor = Vec::new();
    for &field in F::NUMERIC {
        let bounds = field.bounds().expect("a numeric field");
        assert_eq!(
            attempt(field, bounds.default, &[]),
            Ok(bounds.default),
            "{field}'s default builds"
        );
        if bounds.min > 0 {
            assert_eq!(
                attempt(field, bounds.min - 1, &[]),
                Err(TracePolicyError::Range {
                    field,
                    value: bounds.min - 1,
                    min: bounds.min,
                    max: bounds.max,
                }),
                "{field} below its range"
            );
        } else {
            // An unsigned field whose minimum is zero has no value below it.
            unsigned_floor.push(field);
        }
        assert_eq!(
            attempt(field, bounds.max + 1, &[]),
            Err(TracePolicyError::Range {
                field,
                value: bounds.max + 1,
                min: bounds.min,
                max: bounds.max,
            }),
            "{field} above its range"
        );
        for value in [bounds.min, bounds.max] {
            if field == F::AttributesPerSpan && value < widest(&K::ALL).1 {
                // Below the largest reserve the relation refuses it; the
                // smallest legal value is that reserve, checked below.
                continue;
            }
            let (rows, detail) = companions(field, value);
            assert_eq!(
                attempt(field, value, &rows),
                Ok(value),
                "{field} = {value} builds"
            );
            let in_core = standard.value(field).is_some();
            if in_core {
                let limits = if matches!(
                    field,
                    F::MaxPartBytes | F::MaxAttributeBytes | F::MaxProviderBodyBytes
                ) {
                    TraceLimits::standard()
                } else {
                    limits(rows.iter().copied().chain([(field, value)]))?
                };
                let content = if matches!(
                    field,
                    F::MaxPartBytes | F::MaxAttributeBytes | F::MaxProviderBodyBytes
                ) {
                    ContentLimits::new(
                        if field == F::MaxPartBytes {
                            value
                        } else {
                            rows.iter()
                                .find_map(|(f, v)| (*f == F::MaxPartBytes).then_some(*v))
                                .unwrap_or_else(|| default(F::MaxPartBytes))
                        },
                        if field == F::MaxAttributeBytes {
                            value
                        } else {
                            rows.iter()
                                .find_map(|(f, v)| (*f == F::MaxAttributeBytes).then_some(*v))
                                .unwrap_or_else(|| default(F::MaxAttributeBytes))
                        },
                        if field == F::MaxProviderBodyBytes {
                            value
                        } else {
                            rows.iter()
                                .find_map(|(f, v)| (*f == F::MaxProviderBodyBytes).then_some(*v))
                                .unwrap_or_else(|| default(F::MaxProviderBodyBytes))
                        },
                    )?
                } else {
                    ContentLimits::standard()
                };
                let policy = reported(limits, content, detail).await?;
                assert_eq!(
                    policy.value(field),
                    Some(value),
                    "the core reports {field} = {value}"
                );
            }
        }
    }
    assert!(
        unsigned_floor.contains(&F::Links) && !unsigned_floor.contains(&F::AttributesPerSpan),
        "the zero-floored fields are recorded as having no below-range value: {unsigned_floor:?}"
    );

    // Relations: each documented one's smallest violation names both
    // operands and both values.
    let (widest_kind, reserve) = widest(&K::ALL);
    assert_eq!(
        limits([(F::AttributesPerSpan, reserve - 1)]),
        Err(TracePolicyError::Relation {
            field: F::AttributesPerSpan,
            value: PolicyValue::Count(reserve - 1),
            relation: PolicyRelation::AtLeast,
            other: PolicyOperand::Reserve(widest_kind),
            other_value: PolicyValue::Count(reserve),
        }),
        "attributes_per_span one below the largest reserve"
    );
    let at_reserve = limits([
        (F::AttributesPerSpan, reserve),
        (F::ToolSpanAttributes, 0),
        (F::References, 0),
    ])
    .expect("attributes_per_span at the largest reserve builds");
    assert_eq!(
        reported(
            at_reserve,
            ContentLimits::standard(),
            TraceDetailPolicy::standard()
        )
        .await?
        .value(F::AttributesPerSpan),
        Some(reserve)
    );
    assert_eq!(
        limits([
            (F::AttributeValueBytes, 8 * 1024),
            (F::ExtensionBytes, 8 * 1024 - 1)
        ]),
        Err(TracePolicyError::Relation {
            field: F::ExtensionBytes,
            value: PolicyValue::Count(8 * 1024 - 1),
            relation: PolicyRelation::AtLeast,
            other: PolicyOperand::Field(F::AttributeValueBytes),
            other_value: PolicyValue::Count(8 * 1024),
        }),
        "extension_bytes below attribute_value_bytes"
    );
    let tool_reserve = u64::from(attribute_reserve(K::Tool));
    let (content_kind, content_reserve) = widest(&CONTENT_KINDS);
    assert_eq!(
        limits([
            (F::AttributesPerSpan, reserve),
            (F::ToolSpanAttributes, reserve - tool_reserve + 1),
            (F::References, 0),
        ]),
        Err(TracePolicyError::Relation {
            field: F::ToolSpanAttributes,
            value: PolicyValue::Count(reserve - tool_reserve + 1),
            relation: PolicyRelation::AtMost,
            other: PolicyOperand::Remaining(K::Tool),
            other_value: PolicyValue::Count(reserve - tool_reserve),
        }),
        "tool_span_attributes past what the tool span's reserve leaves"
    );
    let per_span = content_reserve + 10;
    assert_eq!(
        limits([
            (F::AttributesPerSpan, per_span),
            (F::ToolSpanAttributes, 0),
            (F::References, 11),
        ]),
        Err(TracePolicyError::Relation {
            field: F::References,
            value: PolicyValue::Count(11),
            relation: PolicyRelation::AtMost,
            other: PolicyOperand::Remaining(content_kind),
            other_value: PolicyValue::Count(10),
        }),
        "references past what the widest content kind's reserve leaves"
    );
    for field in [F::AttributeValueBytes, F::ReferenceBytes] {
        let span = 16 * 1024;
        assert_eq!(
            limits([
                (F::SpanContributionBytes, span),
                (F::ReferenceBytes, 0),
                (field, span + 1)
            ]),
            Err(TracePolicyError::Relation {
                field,
                value: PolicyValue::Count(span + 1),
                relation: PolicyRelation::AtMost,
                other: PolicyOperand::Field(F::SpanContributionBytes),
                other_value: PolicyValue::Count(span),
            }),
            "{field} past span_contribution_bytes"
        );
    }

    // Selections: a container off or skipped, and extended detail for a
    // kind that has none, are structural.
    for kind in K::ALL {
        let off = TraceDetailPolicy::builder()
            .set(kind, TraceDetail::Off)
            .build();
        let skipped = TraceExportPolicy::builder().skip(kind).build();
        if kind.is_container() {
            assert_eq!(
                off,
                Err(TracePolicyError::Structural {
                    field: F::Detail(kind),
                    selection: TracePolicySelection::Off,
                }),
                "{kind} turned off"
            );
            assert_eq!(
                skipped,
                Err(TracePolicyError::Structural {
                    field: F::Export(kind),
                    selection: TracePolicySelection::Off,
                }),
                "{kind} skipped"
            );
        }
        let extended = TraceDetailPolicy::builder()
            .set(kind, TraceDetail::Extended)
            .build();
        if matches!(kind, K::Model | K::Workflow) {
            let policy = extended.expect("a kind with extended detail");
            assert_eq!(
                reported(TraceLimits::standard(), ContentLimits::standard(), policy)
                    .await?
                    .detail
                    .get(kind),
                TraceDetail::Extended
            );
        } else {
            assert_eq!(
                extended,
                Err(TracePolicyError::Structural {
                    field: F::Detail(kind),
                    selection: TracePolicySelection::Extended,
                }),
                "{kind} has no extended detail"
            );
        }
    }
    Ok(())
}

/// L21(c) without its selector clause: a kind off, or skipped, while a kind
/// in its `children()` is on is refused at build, naming the parent's and
/// the child's fields and values; and every leaf off with its children
/// builds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_kind_is_off_while_a_kind_it_can_parent_is_on() -> lash::Result<()> {
    let off_parent = |parent: K, child: K, child_value: TraceDetail| TracePolicyError::Relation {
        field: F::Detail(parent),
        value: PolicyValue::Detail(TraceDetail::Off),
        relation: PolicyRelation::ParentOffChildOn,
        other: PolicyOperand::Field(F::Detail(child)),
        other_value: PolicyValue::Detail(child_value),
    };
    assert_eq!(
        TraceDetailPolicy::builder()
            .set(K::Extension, TraceDetail::Off)
            .build(),
        Err(off_parent(K::Extension, K::Model, TraceDetail::Standard)),
        "an extension span off while the model calls under it are on"
    );
    assert_eq!(
        TraceDetailPolicy::builder()
            .set(K::Extension, TraceDetail::Off)
            .set(K::Model, TraceDetail::Off)
            .set(K::ModelAttempt, TraceDetail::Off)
            .build(),
        Err(off_parent(K::Extension, K::Point, TraceDetail::Standard)),
        "an extension span off while its points are on"
    );
    assert_eq!(
        TraceDetailPolicy::builder()
            .set(K::Model, TraceDetail::Off)
            .build(),
        Err(off_parent(K::Model, K::ModelAttempt, TraceDetail::Standard)),
        "a model call off while its attempts are on"
    );
    assert_eq!(
        TraceExportPolicy::builder().skip(K::Extension).build(),
        Err(TracePolicyError::Relation {
            field: F::Export(K::Extension),
            value: PolicyValue::Exported(false),
            relation: PolicyRelation::ParentOffChildOn,
            other: PolicyOperand::Field(F::Export(K::Model)),
            other_value: PolicyValue::Exported(true),
        }),
        "an extension span skipped while the model calls under it are exported"
    );
    let no_extensions = TraceLimits::builder()
        .extension_spans(0)
        .build()
        .expect("the limits alone are legal");
    let refused = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_sessions(false)
    .trace_limits(no_extensions)
    .build(lash_facade_tests::runtime_lease_owner())
    .err()
    .expect("extension_spans = 0 with model calls on is refused");
    assert!(
        matches!(
            &refused,
            lash::EmbedError::TracePolicy(TracePolicyError::Relation {
                field: F::ExtensionSpans,
                value: PolicyValue::Count(0),
                relation: PolicyRelation::ParentOffChildOn,
                other: PolicyOperand::Field(F::Detail(K::Model)),
                other_value: PolicyValue::Detail(TraceDetail::Standard),
            })
        ),
        "{refused:?}"
    );
    // Every leaf off together with what it can parent builds.
    let leaves_off = TraceDetailPolicy::builder()
        .set(K::Extension, TraceDetail::Off)
        .set(K::Point, TraceDetail::Off)
        .set(K::Model, TraceDetail::Off)
        .set(K::ModelAttempt, TraceDetail::Off)
        .build()
        .expect("leaves off with their children");
    let exported_leaves = TraceExportPolicy::builder()
        .skip(K::Extension)
        .skip(K::Point)
        .skip(K::Model)
        .skip(K::ModelAttempt)
        .build()
        .expect("leaves skipped with their children");
    for kind in [K::Extension, K::Point, K::Model, K::ModelAttempt] {
        assert!(!leaves_off.builds(kind));
        assert!(!exported_leaves.exports(kind));
    }
    let policy = reported(no_extensions, ContentLimits::standard(), leaves_off).await?;
    assert_eq!(policy.value(F::ExtensionSpans), Some(0));
    Ok(())
}

/// One moved policy's field table: below `min`, above `max`, and both
/// endpoints, through `attempt`, which builds the policy with the field set
/// and answers the value it reports or the field and bounds it refused.
fn moved_fields<Fld: Copy + std::fmt::Display>(
    fields: &[Fld],
    bounds: impl Fn(Fld) -> lash_core_store::execution_limits::LimitBounds,
    attempt: impl Fn(Fld, u64) -> std::result::Result<u64, (Fld, u64, u64, u64)>,
    same: impl Fn(Fld, Fld) -> bool,
) {
    for &field in fields {
        let lash_core_store::execution_limits::LimitBounds { default, min, max } = bounds(field);
        assert_eq!(
            attempt(field, default).ok(),
            Some(default),
            "{field}'s default builds"
        );
        for value in [min, max] {
            assert_eq!(
                attempt(field, value).ok(),
                Some(value),
                "{field} = {value} builds"
            );
        }
        let mut refused = vec![max + 1];
        if min > 0 {
            refused.push(min - 1);
        }
        for value in refused {
            let (named, at, low, high) = attempt(field, value)
                .err()
                .unwrap_or_else(|| panic!("{field} = {value} is refused"));
            assert!(same(named, field), "the refusal names {field}");
            assert_eq!((at, low, high), (value, min, max), "{field} = {value}");
        }
    }
}

/// L20 over the policies that moved out of `TraceLimits`: every field of
/// `EvidenceLimits`, `DiagnosticLimits` and `CompositionCacheLimits` is
/// refused typed outside its documented range and builds at both ends.
#[test]
fn every_moved_limit_is_refused_typed_out_of_range_and_builds_at_its_endpoints() {
    use lash_core_store::execution_limits::CompositionCacheLimits;
    use lash_core_store::execution_limits::CompositionCacheLimitsError;
    use lash_core_store::execution_limits::CompositionCacheLimitsField as C;
    use lash_core_store::execution_limits::DiagnosticLimits;
    use lash_core_store::execution_limits::DiagnosticLimitsError;
    use lash_core_store::execution_limits::DiagnosticLimitsField as D;
    use lash_core_store::execution_limits::EvidenceLimits;
    use lash_core_store::execution_limits::EvidenceLimitsError;
    use lash_core_store::execution_limits::EvidenceLimitsField as E;
    let size = |value: usize| u64::try_from(value).expect("a size");
    moved_fields(
        E::ALL,
        E::bounds,
        |field, value| {
            let builder = EvidenceLimits::builder();
            let builder = match field {
                E::AttemptStreamBytes => builder.attempt_stream_bytes(value),
                E::DiffValueJsonBytes => builder.diff_value_json_bytes(value),
                E::DiffSummaryPaths => builder.diff_summary_paths(value),
                E::FailurePartialOutputBytes => builder.failure_partial_output_bytes(value),
                E::ProcessEffectOccurrences => builder.process_effect_occurrences(value),
                other => panic!("the law has no dispatch for {other}"),
            };
            match builder.build() {
                Ok(limits) => Ok(match field {
                    E::AttemptStreamBytes => size(limits.attempt_stream_bytes()),
                    E::DiffValueJsonBytes => size(limits.diff_value_json_bytes()),
                    E::DiffSummaryPaths => size(limits.diff_summary_paths()),
                    E::FailurePartialOutputBytes => size(limits.failure_partial_output_bytes()),
                    _ => limits.process_effect_occurrences(),
                }),
                Err(EvidenceLimitsError::Range {
                    field,
                    value,
                    min,
                    max,
                }) => Err((field, value, min, max)),
                Err(other) => panic!("{other}"),
            }
        },
        |left, right| left == right,
    );
    moved_fields(
        C::ALL,
        C::bounds,
        |field, value| {
            let builder = CompositionCacheLimits::builder();
            let builder = match field {
                C::FingerprintGenerations => builder.fingerprint_generations(value),
                C::Sessions => builder.sessions(value),
                other => panic!("the law has no dispatch for {other}"),
            };
            match builder.build() {
                Ok(limits) => Ok(match field {
                    C::FingerprintGenerations => size(limits.fingerprint_generations()),
                    _ => size(limits.sessions()),
                }),
                Err(CompositionCacheLimitsError::Range {
                    field,
                    value,
                    min,
                    max,
                }) => Err((field, value, min, max)),
                Err(other) => panic!("{other}"),
            }
        },
        |left, right| left == right,
    );
    moved_fields(
        D::ALL,
        D::bounds,
        |field, value| {
            let builder = DiagnosticLimits::builder();
            let builder = match field {
                D::DiagnosticErrorChars => builder.diagnostic_error_chars(value),
                D::PluginStateWarnBytes => builder.plugin_state_warn_bytes(Some(value)),
                other => panic!("the law has no dispatch for {other}"),
            };
            match builder.build() {
                Ok(limits) => Ok(match field {
                    D::DiagnosticErrorChars => size(limits.diagnostic_error_chars()),
                    _ => limits
                        .plugin_state_warn_bytes()
                        .map_or(0, std::num::NonZeroU64::get),
                }),
                Err(DiagnosticLimitsError::Range {
                    field,
                    value,
                    min,
                    max,
                }) => Err((field, value, min, max)),
                Err(other) => panic!("{other}"),
            }
        },
        |left, right| left == right,
    );
    // The plugin-state warning is also off when unset.
    let off = DiagnosticLimits::builder()
        .plugin_state_warn_bytes(None)
        .build()
        .expect("no warning builds");
    assert_eq!(off.plugin_state_warn_bytes(), None);
}
