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
    out.push_str("\n### Callback slot scopes\n\nGenerated from `callback_slots!` in `lash-core-store/src/tool_run/state_command.rs`.\n\n| Slot | Scope |\n|---|---|\n");
    let slots = source("crates/lash-core-store/src/tool_run/state_command.rs");
    let slots = slots
        .split_once("callback_slots! {\n")
        .unwrap_or_else(|| panic!("canonical callback_slots table exists"))
        .1
        .split_once("\n}\n")
        .unwrap_or_else(|| panic!("canonical table has a closing brace"))
        .0;
    for line in slots.lines().map(str::trim) {
        if let Some((slot, row)) = line.split_once(" => ") {
            let scope = row
                .rsplit_once(", ")
                .unwrap_or_else(|| panic!("callback row declares a trace scope"))
                .1
                .trim_end_matches(';');
            writeln!(out, "| `{slot}` | `{scope}` |")
                .unwrap_or_else(|error| panic!("write generated reference: {error}"));
        }
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
