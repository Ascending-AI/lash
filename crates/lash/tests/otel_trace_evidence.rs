//! Compile witnesses for the host-installed telemetry adapter and its provider API.
#![cfg(feature = "otel-trace")]
#![allow(dead_code)]

use lash::tracing::{
    OtelOptions, OtelTelemetry, SpanEnricher, SpanEnrichment, SpanView, contract_markdown, otel,
};
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

/// The public instrumentation contract is generated from the typed span,
/// attribute and metric registry in `lash-trace`; the checked-in document is
/// the reference a host diffs its exported shape against. Regenerate it with:
///
/// ```sh
/// kiln test //crates/lash:otel_trace_evidence__test --local-test-execution \
///     --test_arg=--ignored \
///     --test_arg=--exact \
///     --test_arg=instrumentation_contract_document_is_regenerated \
///     --test_env=BUILD_WORKSPACE_DIRECTORY=$PWD
/// ```
#[test]
fn instrumentation_contract_document_matches_the_registry() {
    assert_eq!(
        contract_markdown(),
        include_str!("../docs/instrumentation-contract.md"),
        "docs/instrumentation-contract.md is stale; regenerate it with the ignored writer below"
    );
}

#[test]
#[ignore = "writes crates/lash/docs/instrumentation-contract.md"]
#[allow(
    clippy::disallowed_methods,
    reason = "the ignored regeneration writer writes the checked-in document into the real checkout"
)]
fn instrumentation_contract_document_is_regenerated() {
    let output = match std::env::var_os("BUILD_WORKSPACE_DIRECTORY") {
        Some(root) => PathBuf::from(root).join("crates/lash/docs/instrumentation-contract.md"),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/instrumentation-contract.md"),
    };
    assert!(
        output.is_absolute(),
        "regeneration needs the real checkout: run the test with BUILD_WORKSPACE_DIRECTORY set to the repository root"
    );
    std::fs::write(&output, contract_markdown())
        .unwrap_or_else(|error| panic!("write {}: {error}", output.display()));
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

/// Extracts the rustdoc attached to an item, including its attributes.
fn item_doc(text: &str, item: &str) -> String {
    let before = text
        .split_once(item)
        .unwrap_or_else(|| panic!("missing {item}"))
        .0;
    let lines: Vec<_> = before.lines().collect();
    let end = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with("///"))
        .unwrap_or_else(|| panic!("missing rustdoc for {item}"));
    let start = lines[..=end]
        .iter()
        .rposition(|line| !line.trim_start().starts_with("///"))
        .map_or(0, |i| i + 1);
    lines[start..=end]
        .iter()
        .map(|line| {
            line.trim_start()
                .strip_prefix("///")
                .unwrap_or_else(|| panic!("selected line is rustdoc"))
                .trim_start()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn configuration_markdown() -> String {
    use lash::tracing::{TracePolicyField, TraceSpanKind, attribute_reserve};
    use std::fmt::Write;
    let mut out = String::from(
        "<!-- BEGIN GENERATED CONFIGURATION -->\n\n\
Generated from policy rustdoc and its numeric field registry by\n\
`tracing_configuration_is_regenerated` in `otel_trace_evidence.rs`.\n\
Numeric sizes are bytes, durations milliseconds, other values counts.\n\n",
    );
    for (path, item, title) in [
        (
            "content.rs",
            "pub struct ContentPolicy",
            "ContentPolicy (core ceiling)",
        ),
        (
            "content/selector.rs",
            "pub trait CaptureSelector",
            "CaptureSelector (core, per record)",
        ),
        ("content.rs", "pub trait ContentHook", "ContentHook (core)"),
        (
            "policy.rs",
            "pub struct ContentLimits",
            "ContentLimits (core)",
        ),
        (
            "policy.rs",
            "pub const fn standard()",
            "ContentLimits defaults",
        ),
        (
            "policy.rs",
            "pub fn new(",
            "ContentLimits bounds and relations",
        ),
        (
            "policy/detail.rs",
            "pub struct TraceDetailPolicy",
            "TraceDetailPolicy (core)",
        ),
        (
            "policy/detail.rs",
            "pub fn build(self)",
            "TraceDetailPolicy refusals",
        ),
        (
            "policy/detail.rs",
            "pub struct TraceExportPolicy",
            "TraceExportPolicy (exporter)",
        ),
        (
            "policy/detail.rs",
            "pub enum TraceMetrics",
            "TraceMetrics (exporter)",
        ),
        (
            "otel.rs",
            "pub struct OtelOptions",
            "OtelOptions (exporter)",
        ),
        ("otel.rs", "pub fn standard()", "OtelOptions defaults"),
        (
            "policy/limits.rs",
            "pub struct TraceLimits",
            "TraceLimits (core)",
        ),
        (
            "telemetry.rs",
            "pub struct TraceStateLimits",
            "TraceStateLimits (host ingress)",
        ),
        (
            "telemetry.rs",
            "pub const fn standard()",
            "TraceStateLimits defaults",
        ),
        (
            "flight_recorder.rs",
            "pub struct FlightRecorderSettings",
            "FlightRecorderSettings (host sink)",
        ),
        (
            "flight_recorder.rs",
            "pub fn standard()",
            "FlightRecorderSettings defaults",
        ),
        (
            "flight_recorder.rs",
            "pub fn new(",
            "FlightRecorderSettings bounds",
        ),
    ] {
        let rustdoc = item_doc(&source(&format!("crates/lash-trace/src/{path}")), item);
        // Rustdoc shorthand references render as code in the Markdown guide.
        let rustdoc = rustdoc.replace("[`", "`").replace("`]", "`");
        writeln!(out, "### {title}\n\n{rustdoc}\n")
            .unwrap_or_else(|error| panic!("write generated reference: {error}"));
    }
    out.push_str("### TraceLimits field contracts\n\n");
    let limits = source("crates/lash-trace/src/policy/limits.rs");
    let rows = limits
        .split_once("trace_limits! {\n")
        .unwrap_or_else(|| panic!("canonical trace_limits table exists"))
        .1
        .split_once("\n}\n")
        .unwrap_or_else(|| panic!("canonical table has a closing brace"))
        .0;
    for line in rows.lines().map(str::trim) {
        if let Some(doc) = line.strip_prefix('"').and_then(|s| s.strip_suffix("\";")) {
            writeln!(out, "- {doc}")
                .unwrap_or_else(|error| panic!("write generated reference: {error}"));
        }
    }
    out.push_str("\n### Numeric defaults and inclusive bounds\n\n| Field | Default | Min | Max |\n|---|---|---|---|\n");
    for field in TracePolicyField::NUMERIC {
        let b = field
            .bounds()
            .unwrap_or_else(|| panic!("numeric policy field has bounds"));
        writeln!(out, "| `{field}` | {} | {} | {} |", b.default, b.min, b.max)
            .unwrap_or_else(|error| panic!("write generated reference: {error}"));
    }
    out.push_str(
        "\n### Registry attribute reserves\n\n| Span kind | attribute_reserve |\n|---|---|\n",
    );
    for kind in TraceSpanKind::ALL {
        writeln!(out, "| `{kind}` | {} |", attribute_reserve(kind))
            .unwrap_or_else(|error| panic!("write generated reference: {error}"));
    }
    out.push_str("\n### Callback slot scopes\n\nGenerated from `CallbackSlot::ALL` and `CallbackSlot::trace_scope` in `lash-vocabulary/src/providers.rs`.\n\n| Slot | Scope |\n|---|---|\n");
    for slot in lash::plugins::CallbackSlot::ALL {
        writeln!(out, "| `{slot:?}` | `{:?}` |", slot.trace_scope())
            .unwrap_or_else(|error| panic!("write generated reference: {error}"));
    }
    out.push_str("\n<!-- END GENERATED CONFIGURATION -->");
    out
}

#[test]
fn tracing_configuration_matches_policy_rustdoc_and_registry() {
    let doc = source("docs/architecture/tracing.md");
    let snapshot = configuration_markdown();
    assert!(
        doc.contains(&snapshot),
        "Configuration is stale; run tracing_configuration_is_regenerated"
    );
}

#[test]
#[ignore = "writes the generated Configuration section in docs/architecture/tracing.md"]
#[allow(
    clippy::disallowed_methods,
    reason = "explicit recorder updates the checked-in documentation"
)]
fn tracing_configuration_is_regenerated() {
    let path = repository().join("docs/architecture/tracing.md");
    let doc = source("docs/architecture/tracing.md");
    let (before, rest) = doc
        .split_once("<!-- BEGIN GENERATED CONFIGURATION -->")
        .expect("configuration start marker");
    let (_, after) = rest
        .split_once("<!-- END GENERATED CONFIGURATION -->")
        .expect("configuration end marker");
    std::fs::write(path, format!("{before}{}{after}", configuration_markdown())).unwrap();
}

/// N6: production hosts cannot inject a second identity/projector runtime.
#[test]
fn composed_trace_runtime_installation_is_test_only() {
    let core = source("crates/lash/src/core/trace_config.rs");
    let declaration = core
        .split_once("pub fn trace_runtime(")
        .expect("test installer")
        .0;
    let attributes = declaration.rsplit_once("\n    }").unwrap().1;
    assert!(
        attributes.contains("#[cfg(feature = \"testing\")]"),
        "composed runtime installation must be testing-only"
    );
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
        resource: lash_protocol_types::CellResource::MemoryBytes,
        limit: 100,
        observed: Some(101),
        consumed: Some(101),
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
    let mut failure = lash_protocol_types::CellFailure::new(
        lash_protocol_types::CellFailureKind::Policy,
        "bound",
    );
    failure.bound = Some(Box::new(fact));
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
