//! Compile witnesses for the host-installed telemetry adapter and its provider API.
#![cfg(feature = "otel-trace")]
#![allow(dead_code)]

use lash::tracing::{
    GEN_AI_SEMCONV_SNAPSHOT, LASH_INSTRUMENTATION_CONTRACT, LASH_INSTRUMENTATION_NAME, OtelOptions,
    OtelPayloadExport, OtelSpanEnricher, OtelTelemetry, contract_markdown, otel,
};
use std::path::PathBuf;
use std::sync::Arc;

struct Enricher;
impl OtelSpanEnricher for Enricher {
    fn attributes(&self, _: &lash::tracing::TraceRecord, out: &mut Vec<otel::KeyValue>) {
        out.push(otel::KeyValue::new("host.attribute", true));
    }
}

fn install<P: otel::trace::TracerProvider, M: otel::metrics::MeterProvider>(
    builder: lash::LashCoreBuilder,
    tracer: &P,
    meter: &M,
) -> lash::LashCoreBuilder
where
    P::Tracer: Send + Sync + 'static,
    <P::Tracer as otel::trace::Tracer>::Span: Send + Sync + 'static,
{
    let telemetry = OtelTelemetry::new(
        tracer,
        meter,
        OtelOptions {
            include_context_metadata: false,
            payloads: OtelPayloadExport::Bounded {
                max_record_bytes: 256,
                max_events: 2,
            },
            enrich: Some(Arc::new(Enricher)),
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
/// kiln test //crates/lash:otel_trace_evidence__test__fv_225e6839 \
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

/// The contract document's header names the pinned scope, contract version and
/// GenAI semconv snapshot through the facade, so a host can check the values it
/// wires into its own providers against the same constants.
#[test]
fn instrumentation_contract_declares_the_shared_scope_and_snapshot() {
    let document = contract_markdown();
    for constant in [
        LASH_INSTRUMENTATION_NAME,
        LASH_INSTRUMENTATION_CONTRACT,
        GEN_AI_SEMCONV_SNAPSHOT,
    ] {
        assert!(
            document.contains(constant),
            "the generated contract names {constant}"
        );
    }
}
