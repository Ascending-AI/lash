//! Compile-time witnesses for store conformance support contracts.
//!
//! These probes type-check public contracts without constructing a live backend.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables)]

mod facade_support {
    pub use lash_core::facade_support::*;
}

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
    // FIG-2105-TEST-WITNESS-0013: lash_conformance::FenceIntegrityTarget::QueuedWorkClaimFence [variant]
    variant_witness(|value: &lash_conformance::FenceIntegrityTarget| {
        matches!(
            value,
            lash_conformance::FenceIntegrityTarget::QueuedWorkClaimFence { .. }
        )
    });
    // FIG-2105-TEST-WITNESS-0014: lash_conformance::FenceIntegrityTarget::QueuedWorkClaimFence::batch_id [field]
    field_witness(|value: &lash_conformance::FenceIntegrityTarget| {
        if let lash_conformance::FenceIntegrityTarget::QueuedWorkClaimFence { batch_id, .. } = value
        {
            let _ = batch_id;
        }
    });
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
    // FIG-2105-TEST-WITNESS-0017: lash_conformance::FenceIntegrityTarget::SessionLeaseFencingToken [variant]
    variant_witness(|value: &lash_conformance::FenceIntegrityTarget| {
        matches!(
            value,
            lash_conformance::FenceIntegrityTarget::SessionLeaseFencingToken { .. }
        )
    });
    // FIG-2105-TEST-WITNESS-0018: lash_conformance::FenceIntegrityTarget::SessionLeaseFencingToken::session_id [field]
    field_witness(|value: &lash_conformance::FenceIntegrityTarget| {
        if let lash_conformance::FenceIntegrityTarget::SessionLeaseFencingToken {
            session_id, ..
        } = value
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
    // FIG-2105-TEST-WITNESS-0021: lash_conformance::SessionExecutionLeaseRenewalZeroRowHandles [struct]
    type_witness::<lash_conformance::SessionExecutionLeaseRenewalZeroRowHandles>();
    // FIG-2105-TEST-WITNESS-0022: lash_conformance::SessionExecutionLeaseRenewalZeroRowHandles::injector [field]
    field_witness(
        |value: &lash_conformance::SessionExecutionLeaseRenewalZeroRowHandles| {
            let _ = &value.injector;
        },
    );
    // FIG-2105-TEST-WITNESS-0023: lash_conformance::SessionExecutionLeaseRenewalZeroRowHandles::store [field]
    field_witness(
        |value: &lash_conformance::SessionExecutionLeaseRenewalZeroRowHandles| {
            let _ = &value.store;
        },
    );
    // FIG-2105-TEST-WITNESS-0024: lash_conformance::SessionExecutionLeaseRenewalZeroRowInjector [trait]
    fn trait_witness_0024<T: lash_conformance::SessionExecutionLeaseRenewalZeroRowInjector>() {}
    // FIG-2105-TEST-WITNESS-0025: lash_conformance::SessionExecutionLeaseRenewalZeroRowInjector::arm [function]
    fn method_witness_0025<T: lash_conformance::SessionExecutionLeaseRenewalZeroRowInjector>(
        value: &T,
    ) {
        std::mem::drop(
            lash_conformance::SessionExecutionLeaseRenewalZeroRowInjector::arm(
                value,
                &SessionId::from("session"),
            ),
        );
    }
    // FIG-2105-TEST-WITNESS-0026: lash_conformance::SessionExecutionLeaseRenewalZeroRowInjector::disarm [function]
    fn method_witness_0026<T: lash_conformance::SessionExecutionLeaseRenewalZeroRowInjector>(
        value: &T,
    ) {
        std::mem::drop(
            lash_conformance::SessionExecutionLeaseRenewalZeroRowInjector::disarm(value),
        );
    }
    // FIG-2105-TEST-WITNESS-0027: lash_conformance::UnboundSessionAdmissionState [enum]
    type_witness::<lash_conformance::UnboundSessionAdmissionState>();
    // FIG-2105-TEST-WITNESS-0028: lash_conformance::UnboundSessionAdmissionState::AdmittedOnly [variant]
    variant_witness(|value: &lash_conformance::UnboundSessionAdmissionState| {
        matches!(
            value,
            lash_conformance::UnboundSessionAdmissionState::AdmittedOnly
        )
    });
    // FIG-2105-TEST-WITNESS-0029: lash_conformance::UnboundSessionAdmissionState::Committed [variant]
    variant_witness(|value: &lash_conformance::UnboundSessionAdmissionState| {
        matches!(
            value,
            lash_conformance::UnboundSessionAdmissionState::Committed
        )
    });
    // FIG-2105-TEST-WITNESS-0030: lash_conformance::UnboundSessionResolutionHandles [struct]
    type_witness::<lash_conformance::UnboundSessionResolutionHandles>();
    // FIG-2105-TEST-WITNESS-0031: lash_conformance::UnboundSessionResolutionHandles::backend_name [field]
    field_witness(
        |value: &lash_conformance::UnboundSessionResolutionHandles| {
            let _ = &value.backend_name;
        },
    );
    // FIG-2105-TEST-WITNESS-0032: lash_conformance::UnboundSessionResolutionHandles::factory [field]
    field_witness(
        |value: &lash_conformance::UnboundSessionResolutionHandles| {
            let _ = &value.factory;
        },
    );
    // FIG-2105-TEST-WITNESS-0033: lash_conformance::UnboundSessionResolutionHandles::open_unbound [field]
    field_witness(
        |value: &lash_conformance::UnboundSessionResolutionHandles| {
            let _ = &value.open_unbound;
        },
    );
    // FIG-2105-TEST-WITNESS-0034: lash_core::facade_support::RuntimeSessionStateFacadeOps::turn_state [function]
    fn method_witness_0034<T: crate::facade_support::RuntimeSessionStateFacadeOps>(value: &T) {
        let _ = crate::facade_support::RuntimeSessionStateFacadeOps::turn_state(value);
    }
    // FIG-2105-TEST-WITNESS-0035: lash_core::facade_support::SessionGraphFacadeOps::active_path_nodes [function]
    fn method_witness_0035<T: crate::facade_support::SessionGraphFacadeOps>(value: &T) {
        let _ = crate::facade_support::SessionGraphFacadeOps::active_path_nodes(value);
    }
    // FIG-2105-TEST-WITNESS-0037: lash_conformance::GraphIntegrityCorruption [enum]
    type_witness::<lash_conformance::GraphIntegrityCorruption>();
    // FIG-2105-TEST-WITNESS-0038: lash_conformance::GraphIntegrityCorruption::DanglingLeafId [variant]
    variant_witness(|value: &lash_conformance::GraphIntegrityCorruption| {
        matches!(
            value,
            lash_conformance::GraphIntegrityCorruption::DanglingLeafId
        )
    });
    // FIG-2105-TEST-WITNESS-0039: lash_conformance::GraphIntegrityCorruption::DuplicateNodeId [variant]
    variant_witness(|value: &lash_conformance::GraphIntegrityCorruption| {
        matches!(
            value,
            lash_conformance::GraphIntegrityCorruption::DuplicateNodeId
        )
    });
    // FIG-2105-TEST-WITNESS-0040: lash_conformance::GraphIntegrityCorruption::OrphanLeaf [variant]
    variant_witness(|value: &lash_conformance::GraphIntegrityCorruption| {
        matches!(
            value,
            lash_conformance::GraphIntegrityCorruption::OrphanLeaf
        )
    });
    // FIG-2105-TEST-WITNESS-0041: lash_conformance::GraphIntegrityCorruption::ParentCycle [variant]
    variant_witness(|value: &lash_conformance::GraphIntegrityCorruption| {
        matches!(
            value,
            lash_conformance::GraphIntegrityCorruption::ParentCycle
        )
    });
    // FIG-2105-TEST-WITNESS-0042: lash_conformance::GraphIntegrityHandles [struct]
    type_witness::<lash_conformance::GraphIntegrityHandles>();
    // FIG-2105-TEST-WITNESS-0043: lash_conformance::GraphIntegrityHandles::injector [field]
    field_witness(|value: &lash_conformance::GraphIntegrityHandles| {
        let _ = &value.injector;
    });
    // FIG-2105-TEST-WITNESS-0044: lash_conformance::GraphIntegrityHandles::runtime [field]
    field_witness(|value: &lash_conformance::GraphIntegrityHandles| {
        let _ = &value.runtime;
    });
    // FIG-2105-TEST-WITNESS-0045: lash_conformance::GraphIntegrityInjector [trait]
    fn trait_witness_0045<T: lash_conformance::GraphIntegrityInjector>() {}
    // FIG-2105-TEST-WITNESS-0046: lash_conformance::GraphIntegrityInjector::cleanup [function]
    fn method_witness_0046<T: lash_conformance::GraphIntegrityInjector>(
        value: &T,
        target: &lash_conformance::GraphIntegrityTarget,
    ) {
        std::mem::drop(lash_conformance::GraphIntegrityInjector::cleanup(
            value, target,
        ));
    }
    // FIG-2105-TEST-WITNESS-0047: lash_conformance::GraphIntegrityInjector::inject [function]
    fn method_witness_0047<T: lash_conformance::GraphIntegrityInjector>(
        value: &T,
        target: &lash_conformance::GraphIntegrityTarget,
    ) {
        std::mem::drop(lash_conformance::GraphIntegrityInjector::inject(
            value, target,
        ));
    }
    // FIG-2105-TEST-WITNESS-0048: lash_conformance::GraphIntegrityInjector::load_whole_graph [function]
    fn method_witness_0048<T: lash_conformance::GraphIntegrityInjector>(value: &T) {
        std::mem::drop(lash_conformance::GraphIntegrityInjector::load_whole_graph(
            value,
            &SessionId::from("session"),
        ));
    }
    // FIG-2105-TEST-WITNESS-0049: lash_conformance::GraphIntegrityRead [enum]
    type_witness::<lash_conformance::GraphIntegrityRead>();
    // FIG-2105-TEST-WITNESS-0050: lash_conformance::GraphIntegrityRead::ActivePath [variant]
    variant_witness(|value: &lash_conformance::GraphIntegrityRead| {
        matches!(value, lash_conformance::GraphIntegrityRead::ActivePath)
    });
    // FIG-2105-TEST-WITNESS-0051: lash_conformance::GraphIntegrityRead::WholeGraph [variant]
    variant_witness(|value: &lash_conformance::GraphIntegrityRead| {
        matches!(value, lash_conformance::GraphIntegrityRead::WholeGraph)
    });
    // FIG-2105-TEST-WITNESS-0052: lash_conformance::GraphIntegrityTarget [struct]
    type_witness::<lash_conformance::GraphIntegrityTarget>();
    // FIG-2105-TEST-WITNESS-0053: lash_conformance::GraphIntegrityTarget::corruption [field]
    field_witness(|value: &lash_conformance::GraphIntegrityTarget| {
        let _ = &value.corruption;
    });
    // FIG-2105-TEST-WITNESS-0054: lash_conformance::GraphIntegrityTarget::leaf_node_id [field]
    field_witness(|value: &lash_conformance::GraphIntegrityTarget| {
        let _ = &value.leaf_node_id;
    });
    // FIG-2105-TEST-WITNESS-0055: lash_conformance::GraphIntegrityTarget::missing_node_id [field]
    field_witness(|value: &lash_conformance::GraphIntegrityTarget| {
        let _ = &value.missing_node_id;
    });
    // FIG-2105-TEST-WITNESS-0056: lash_conformance::GraphIntegrityTarget::read [field]
    field_witness(|value: &lash_conformance::GraphIntegrityTarget| {
        let _ = &value.read;
    });
    // FIG-2105-TEST-WITNESS-0057: lash_conformance::GraphIntegrityTarget::root_node_id [field]
    field_witness(|value: &lash_conformance::GraphIntegrityTarget| {
        let _ = &value.root_node_id;
    });
    // FIG-2105-TEST-WITNESS-0058: lash_conformance::GraphIntegrityTarget::session_id [field]
    field_witness(|value: &lash_conformance::GraphIntegrityTarget| {
        let _ = &value.session_id;
    });
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
}
