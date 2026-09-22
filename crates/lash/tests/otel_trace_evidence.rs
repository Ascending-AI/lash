//! Compile-time witnesses for the `otel-trace`-gated observation-area rows.
//!
//! FIG-2107 drains the ledger's remaining `unused-justify` slices. These seven
//! rows name items that only exist under the `otel-trace` feature, which the
//! `otel-feature-chain` coverage lane owns — the feature-coverage contract
//! forbids combining it with the `testing` feature (a `runtime-features` lane
//! feature) in one predicate, so they cannot live in
//! `observation_admin_evidence.rs` with the rest of their area.

#![cfg(feature = "otel-trace")]
#![allow(dead_code, unreachable_code, unused_variables, unused_imports)]
#![allow(clippy::all)]

fn type_witness<T>() {}
fn field_witness<T>(_: impl FnOnce(&T)) {}

fn drain_area_witnesses() {
    // W0040: lash::tracing::OtelTraceOptions [struct]
    type_witness::<lash::tracing::OtelTraceOptions>();
    // W0041: lash::tracing::OtelTraceOptions::include_context_metadata [field]
    field_witness(|value: &lash::tracing::OtelTraceOptions| {
        let _ = &value.include_context_metadata;
    });
    // W0042: lash::tracing::OtelTraceOptions::include_event_json [field]
    field_witness(|value: &lash::tracing::OtelTraceOptions| {
        let _ = &value.include_event_json;
    });
    // W0043: lash::tracing::OtelTraceOptions::include_payload_json [field]
    field_witness(|value: &lash::tracing::OtelTraceOptions| {
        let _ = &value.include_payload_json;
    });
    // W0044: lash::tracing::OtelTraceSink::new [function]
    let _: fn(_) -> lash::tracing::OtelTraceSink = lash::tracing::OtelTraceSink::new;
    // W0045: lash::tracing::OtelTraceSink::options [function]
    let _: for<'a> fn(&'a lash::tracing::OtelTraceSink) -> &'a lash::tracing::OtelTraceOptions =
        lash::tracing::OtelTraceSink::options;
    // W0046: lash::tracing::OtelTraceSink::with_options [function]
    let _: fn(_, lash::tracing::OtelTraceOptions) -> lash::tracing::OtelTraceSink =
        lash::tracing::OtelTraceSink::with_options;
}
