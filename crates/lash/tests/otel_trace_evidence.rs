//! Compile witnesses for the host-installed telemetry adapter and its provider API.
#![cfg(feature = "otel-trace")]
#![allow(dead_code)]

use lash::tracing::{OtelOptions, OtelTelemetry, SpanEnricher, SpanEnrichment, SpanView, otel};
use std::path::PathBuf;
use std::sync::Arc;

struct Enricher;
impl SpanEnricher for Enricher {
    fn enrich(&self, _: &SpanView<'_>, out: &mut SpanEnrichment) {
        let _refused = out.set("host.attribute", true);
    }
}

fn install<M: otel::metrics::MeterProvider>(
    builder: lash::LashCoreBuilder,
    tracer: &lash::tracing::LashTracerProvider,
    meter: &M,
) -> lash::LashCoreBuilder {
    let telemetry = OtelTelemetry::new(
        tracer,
        meter,
        OtelOptions {
            enricher: Some(Arc::new(Enricher)),
            ..OtelOptions::standard()
        },
    );
    let _: &lash::tracing::TelemetryMetrics = telemetry.metrics();
    let _: &OtelOptions = telemetry.options();
    builder.telemetry(telemetry)
}

// Buck declares the documentation and policy sources as inputs. The ignored
// recorder overrides the root with BUILD_WORKSPACE_DIRECTORY when writing back.
#[allow(
    clippy::disallowed_methods,
    reason = "documentation laws inspect their declared repository inputs"
)]
fn repository() -> PathBuf {
    std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        PathBuf::from,
    )
}

#[allow(
    clippy::disallowed_methods,
    reason = "documentation laws read repository documentation and rustdoc"
)]
fn source(path: &str) -> String {
    std::fs::read_to_string(repository().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn heading_anchor(heading: &str) -> String {
    heading
        .trim_start_matches('#')
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// A host following a local tracing reference must reach a file and section.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "link law reads the referenced documentation files"
)]
fn tracing_documentation_links_resolve() {
    for path in [
        "docs/architecture/tracing.md",
        "docs/plugins.md",
        "docs/agents/logging-and-events.md",
        "docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md",
    ] {
        let doc = source(path);
        for link in doc
            .split("](")
            .skip(1)
            .filter_map(|s| s.split_once(')').map(|p| p.0))
        {
            if link.contains("://") || link.starts_with("mailto:") {
                continue;
            }
            let (file, anchor) = link
                .split_once('#')
                .map_or((link, None), |(f, a)| (f, Some(a)));
            let target = if file.is_empty() {
                repository().join(path)
            } else {
                repository().join(path).parent().unwrap().join(file)
            };
            assert!(target.is_file(), "{path}: broken link {link}");
            if let Some(anchor) = anchor {
                if anchor.starts_with('L') && anchor[1..].chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let text = std::fs::read_to_string(&target).unwrap();
                assert!(
                    text.lines()
                        .filter(|l| l.starts_with('#'))
                        .any(|l| heading_anchor(l) == anchor),
                    "{path}: missing anchor {link}"
                );
            }
        }
    }
}

/// FIG-7557: root process and cell spans retain the same terminal bound
/// even when tool-result content is omitted.
#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn process_and_cell_execution_spans_retain_the_terminal_bound() {
    use lash::tracing::*;
    use opentelemetry_sdk::trace::{
        InMemorySpanExporter, RandomIdGenerator, Sampler, SdkTracerProvider,
    };
    let exporter = InMemorySpanExporter::default();
    let provider = LashTracerProvider::new(
        SdkTracerProvider::builder().with_simple_exporter(exporter.clone()),
        Sampler::AlwaysOn,
        RandomIdGenerator::default(),
    );
    let meter = opentelemetry_sdk::metrics::SdkMeterProvider::builder().build();
    let adapter = OtelTelemetry::new(&provider, &meter, OtelOptions::standard());
    let fact = lash::ExecutionBoundFact {
        reading: lash_protocol_types::CellConsumption {
            resource: lash_protocol_types::CellResource::MemoryBytes,
            limit: 100,
            observed: Some(101),
        },
        function: Some(lash_kernel_value::names::FunctionName::new("text.concat").unwrap()),
        site: None,
    };
    let terminal = lash_protocol_types::ToolFailure::runtime(
        lash_protocol_types::ToolFailureClass::Execution,
        "process_bound_exceeded",
        "bound",
    )
    .with_cause(lash_protocol_types::ToolFailureCause::ExecutionBound { fact: fact.clone() });
    let completion: TraceDomainCompletion = serde_json::from_value(serde_json::json!({
        "subject":{"type":"process","kind":"kernel"}, "started_at_ms":1000, "status":"failed",
        "failure": TraceToolFailure::new(&terminal, lash_trace::ContentPolicy::standard()),
    }))
    .unwrap();
    let failure = lash_protocol_types::CellFailure::from_cause(
        lash_protocol_types::CellFailureCause::Bound {
            fact: Box::new(fact),
            worker_limit: None,
        },
        "bound",
    );
    let events = [
        (
            TraceSpanKind::Process,
            TraceEvent::DomainCompleted { completion },
        ),
        (
            TraceSpanKind::Cell,
            TraceEvent::ExecCodeCompleted {
                consumption: Vec::new(),
                trailing_discarded: 0,
                code: String::new(),
                language: "typescript".into(),
                started_at_ms: 1000,
                duration_ms: 1,
                output: String::new(),
                output_chars: 0,
                observation_count: 0,
                observation_projections: Vec::new(),
                result: lash_protocol_types::CellOutcome::Failed(failure),
                tool_calls: Vec::new(),
            },
        ),
    ];
    let parent = TraceCarrier::parse_w3c(
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        None,
    )
    .unwrap();
    for (ordinal, (kind, event)) in events.into_iter().enumerate() {
        let scope = TraceScopeId::admission(TraceScopeOwner::Turn {
            session_id: "bound-session".into(),
            turn_id: format!("bound-{ordinal}").try_into().unwrap(),
        });
        let identity = TraceRecordIdentity::Transition {
            scope: scope.clone(),
            transition: TraceTransitionKind::Terminal,
            ordinal: 0,
        };
        let at = TraceTimestamp::from_unix_ms(1001);
        let record = TraceRecord::identified(&identity, TraceContext::default(), event, at)
            .unwrap()
            .at(Some(TracePosition {
                span: derive_span(&parent, &SpanKey::Scope { scope: &scope }),
                parent: Some(parent.clone()),
                links: TraceLinks::new(),
                role: SpanRole::Closes,
                kind,
                started_at: TraceTimestamp::from_unix_ms(1000),
                ended_at: Some(at),
                scope: Some(scope),
                attempt: None,
            }))
            .governed(lash_trace::ContentPolicy::standard());
        adapter.append(&record).unwrap();
    }
    provider.provider().force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    let bounds: Vec<_> = spans
        .iter()
        .map(|span| {
            let value = |key: &str| {
                span.attributes
                    .iter()
                    .find(|attribute| attribute.key.as_str() == key)
                    .map(|attribute| attribute.value.to_string())
            };
            (
                value("lash.execution.bound.resource"),
                value("lash.execution.bound.limit"),
                value("lash.execution.bound.observed"),
                value("lash.execution.bound.consumed"),
                value("lash.execution.bound.function"),
                value("lash.execution.bound.site"),
            )
        })
        .collect();
    assert_eq!(
        bounds,
        vec![
            (
                Some("memory_bytes".into()),
                Some("100".into()),
                Some("101".into()),
                Some("101".into()),
                Some("text.concat".into()),
                None
            );
            2
        ]
    );
}
