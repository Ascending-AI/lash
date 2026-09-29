//! Compile-time witnesses for store conformance support contracts.
//!
//! These probes type-check public contracts without constructing a live backend.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables)]

use lash_sansio::SessionId;

type SessionNodeRecord = lash_core::SessionNodeRecord;

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

pub(crate) fn store_area_test_support_witnesses() {
    // FIG-2105-TEST-WITNESS-0001: lash_conformance::StoreRecoveryLeaseTiming::Controlled::0 [field]
    field_witness(|value: &lash_conformance::StoreRecoveryLeaseTiming| {
        if let lash_conformance::StoreRecoveryLeaseTiming::Controlled(field, ..) = value {
            let _ = field;
        }
    });
    // FIG-2105-TEST-WITNESS-0002: lash_conformance::FenceIntegrityHandles [struct]
    type_witness::<lash_conformance::FenceIntegrityHandles>();
    // FIG-2105-TEST-WITNESS-0003: lash_conformance::FenceIntegrityHandles::injector [field]
    field_witness(|value: &lash_conformance::FenceIntegrityHandles| {
        let _ = &value.injector;
    });
    // FIG-2105-TEST-WITNESS-0004: lash_conformance::FenceIntegrityHandles::runtime [field]
    field_witness(|value: &lash_conformance::FenceIntegrityHandles| {
        let _ = &value.runtime;
    });
    // FIG-2105-TEST-WITNESS-0005: lash_conformance::FenceIntegrityHandles::triggers [field]
    field_witness(|value: &lash_conformance::FenceIntegrityHandles| {
        let _ = &value.triggers;
    });
    // FIG-2105-TEST-WITNESS-0006: lash_conformance::FenceIntegrityInjector [trait]
    fn trait_witness_0006<T: lash_conformance::FenceIntegrityInjector>() {}
    // FIG-2105-TEST-WITNESS-0007: lash_conformance::FenceIntegrityInjector::inject_raw_value [function]
    fn method_witness_0007<T: lash_conformance::FenceIntegrityInjector>(
        value: &T,
        target: &lash_conformance::FenceIntegrityTarget,
    ) {
        std::mem::drop(lash_conformance::FenceIntegrityInjector::inject_raw_value(
            value, target, 0,
        ));
    }
    // FIG-2105-TEST-WITNESS-0008: lash_conformance::FenceIntegrityInjector::observe_raw_value [function]
    fn method_witness_0008<T: lash_conformance::FenceIntegrityInjector>(
        value: &T,
        target: &lash_conformance::FenceIntegrityTarget,
    ) {
        std::mem::drop(lash_conformance::FenceIntegrityInjector::observe_raw_value(
            value, target,
        ));
    }
    // FIG-2105-TEST-WITNESS-0009: lash_conformance::FenceIntegrityObservation [struct]
    type_witness::<lash_conformance::FenceIntegrityObservation>();
    // FIG-2105-TEST-WITNESS-0010: lash_conformance::FenceIntegrityObservation::mutation_fingerprint [field]
    field_witness(|value: &lash_conformance::FenceIntegrityObservation| {
        let _ = &value.mutation_fingerprint;
    });
    // FIG-2105-TEST-WITNESS-0011: lash_conformance::FenceIntegrityObservation::value [field]
    field_witness(|value: &lash_conformance::FenceIntegrityObservation| {
        let lash_conformance::FenceIntegrityObservation { value, .. } = value;
        let _ = value;
    });
    // FIG-2105-TEST-WITNESS-0012: lash_conformance::FenceIntegrityTarget [enum]
    type_witness::<lash_conformance::FenceIntegrityTarget>();
    // FIG-2105-TEST-WITNESS-0015: lash_conformance::FenceIntegrityTarget::SessionHeadRevision [variant]
    variant_witness(|value: &lash_conformance::FenceIntegrityTarget| {
        matches!(
            value,
            lash_conformance::FenceIntegrityTarget::SessionHeadRevision { .. }
        )
    });
    // FIG-2105-TEST-WITNESS-0016: lash_conformance::FenceIntegrityTarget::SessionHeadRevision::session_id [field]
    field_witness(|value: &lash_conformance::FenceIntegrityTarget| {
        if let lash_conformance::FenceIntegrityTarget::SessionHeadRevision { session_id, .. } =
            value
        {
            let _ = session_id;
        }
    });
    // FIG-2105-TEST-WITNESS-0019: lash_conformance::FenceIntegrityTarget::TriggerRevision [variant]
    variant_witness(|value: &lash_conformance::FenceIntegrityTarget| {
        matches!(
            value,
            lash_conformance::FenceIntegrityTarget::TriggerRevision { .. }
        )
    });
    // FIG-2105-TEST-WITNESS-0020: lash_conformance::FenceIntegrityTarget::TriggerRevision::subscription_id [field]
    field_witness(|value: &lash_conformance::FenceIntegrityTarget| {
        if let lash_conformance::FenceIntegrityTarget::TriggerRevision {
            subscription_id, ..
        } = value
        {
            let _ = subscription_id;
        }
    });
    // FIG-2105-TEST-WITNESS-0034: lash_core::facade_support::RuntimeSessionStateFacadeOps::turn_state [function]
    fn method_witness_0034<T: crate::facade_support::RuntimeSessionStateFacadeOps>(value: &T) {
        let _ = crate::facade_support::RuntimeSessionStateFacadeOps::turn_state(value);
    }
    // FIG-2105-TEST-WITNESS-0035: lash_core::facade_support::SessionGraphFacadeOps::active_path_nodes [function]
    fn method_witness_0035<T: crate::facade_support::SessionGraphFacadeOps>(value: &T) {
        let _ = crate::facade_support::SessionGraphFacadeOps::active_path_nodes(value);
    }
    // FIG-2105-TEST-WITNESS-0059: lash_conformance::GraphFactObservation [struct]
    type_witness::<lash_conformance::GraphFactObservation>();
    // FIG-2105-TEST-WITNESS-0060: lash_conformance::GraphFactObservation::frame_node_id [field]
    field_witness(|value: &lash_conformance::GraphFactObservation| {
        let _ = &value.frame_node_id;
    });
    // FIG-2105-TEST-WITNESS-0061: lash_conformance::GraphFactObservation::generation [field]
    field_witness(|value: &lash_conformance::GraphFactObservation| {
        let _ = &value.generation;
    });
    // FIG-2105-TEST-WITNESS-0062: lash_conformance::GraphFactObservation::is_frame [field]
    field_witness(|value: &lash_conformance::GraphFactObservation| {
        let _ = &value.is_frame;
    });
    // FIG-2105-TEST-WITNESS-0063: lash_conformance::GraphFactObservation::node_id [field]
    field_witness(|value: &lash_conformance::GraphFactObservation| {
        let _ = &value.node_id;
    });
    // FIG-2105-TEST-WITNESS-0064: lash_conformance::GraphFactObservation::owning_session_id [field]
    field_witness(|value: &lash_conformance::GraphFactObservation| {
        let _ = &value.owning_session_id;
    });
    // FIG-2105-TEST-WITNESS-0065: lash_conformance::GraphFactObservation::parent_node_id [field]
    field_witness(|value: &lash_conformance::GraphFactObservation| {
        let _ = &value.parent_node_id;
    });
    // FIG-2105-TEST-WITNESS-0066: lash_conformance::LineageConformanceHandles [struct]
    type_witness::<lash_conformance::LineageConformanceHandles>();
    // FIG-2105-TEST-WITNESS-0067: lash_conformance::LineageConformanceHandles::factory [field]
    field_witness(|value: &lash_conformance::LineageConformanceHandles| {
        let _ = &value.factory;
    });
    // FIG-2105-TEST-WITNESS-0068: lash_conformance::LineageConformanceHandles::injector [field]
    field_witness(|value: &lash_conformance::LineageConformanceHandles| {
        let _ = &value.injector;
    });
    // FIG-2105-TEST-WITNESS-0069: lash_conformance::LineageConformanceInjector [trait]
    fn trait_witness_0069<T: lash_conformance::LineageConformanceInjector>() {}
    // FIG-2105-TEST-WITNESS-0070: lash_conformance::LineageConformanceInjector::all_graph_facts [function]
    fn method_witness_0070<T: lash_conformance::LineageConformanceInjector>(value: &T) {
        std::mem::drop(lash_conformance::LineageConformanceInjector::all_graph_facts(value));
    }
    // FIG-2105-TEST-WITNESS-0071: lash_conformance::LineageConformanceInjector::edge_path [function]
    fn method_witness_0071<T: lash_conformance::LineageConformanceInjector>(value: &T) {
        std::mem::drop(lash_conformance::LineageConformanceInjector::edge_path(
            value,
            &SessionId::from("session"),
        ));
    }
    // FIG-2105-TEST-WITNESS-0072: lash_conformance::LineageConformanceInjector::force_lineage [function]
    fn method_witness_0072<T: lash_conformance::LineageConformanceInjector>(value: &T) {
        std::mem::drop(lash_conformance::LineageConformanceInjector::force_lineage(
            value,
            &SessionId::from("session"),
            "node",
        ));
    }
    // FIG-2105-TEST-WITNESS-0073: lash_conformance::LineageConformanceInjector::lineage_ancestors [function]
    fn method_witness_0073<T: lash_conformance::LineageConformanceInjector>(value: &T) {
        std::mem::drop(
            lash_conformance::LineageConformanceInjector::lineage_ancestors(
                value,
                &SessionId::from("session"),
            ),
        );
    }
    // FIG-2105-TEST-WITNESS-0074: lash_conformance::LineageConformanceInjector::tombstone_node [function]
    fn method_witness_0074<T: lash_conformance::LineageConformanceInjector>(value: &T) {
        std::mem::drop(lash_conformance::LineageConformanceInjector::tombstone_node(value, "node"));
    }
    // FIG-2107-TEST-WITNESS-0001: lash::persistence::AppendRequestIdentity::Append [variant]
    variant_witness(|value: &lash::persistence::AppendRequestIdentity| {
        matches!(
            value,
            lash::persistence::AppendRequestIdentity::Append { .. }
        )
    });
    // FIG-2107-TEST-WITNESS-0002: lash::persistence::AppendRequestIdentity::PlainCommit [variant]
    variant_witness(|value: &lash::persistence::AppendRequestIdentity| {
        matches!(value, lash::persistence::AppendRequestIdentity::PlainCommit)
    });
    // FIG-2107-TEST-WITNESS-0003: lash::persistence::StoreSchemaVerdict::Migratable [variant]
    variant_witness(|value: &lash::persistence::StoreSchemaVerdict| {
        matches!(
            value,
            lash::persistence::StoreSchemaVerdict::Migratable { .. }
        )
    });
    // FIG-2107-TEST-WITNESS-0004: lash::persistence::StoreSchemaVerdict::Migratable::found [field]
    field_witness(|value: &lash::persistence::StoreSchemaVerdict| {
        if let lash::persistence::StoreSchemaVerdict::Migratable { found, .. } = value {
            let _ = found;
        }
    });
}
