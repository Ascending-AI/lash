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
