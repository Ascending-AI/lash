//! Compile-time witnesses for store conformance support contracts.
//!
//! These probes type-check public contracts without constructing a live backend.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables)]

mod testing {
    pub use lash::testing::*;
}

mod facade_support {
    pub use lash_core::facade_support::*;
}

type SessionNodeRecord = lash_core::SessionNodeRecord;

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

pub(crate) fn store_area_test_support_witnesses() {
    // FIG-2105-TEST-WITNESS-0001: lash::testing::conformance::StoreRecoveryLeaseTiming::Controlled::0 [field]
    field_witness(
        |value: &crate::testing::conformance::StoreRecoveryLeaseTiming| {
            if let crate::testing::conformance::StoreRecoveryLeaseTiming::Controlled(field, ..) =
                value
            {
                let _ = field;
            }
        },
    );
    // FIG-2105-TEST-WITNESS-0002: lash::testing::conformance::FenceIntegrityHandles [struct]
    type_witness::<crate::testing::conformance::FenceIntegrityHandles>();
    // FIG-2105-TEST-WITNESS-0003: lash::testing::conformance::FenceIntegrityHandles::injector [field]
    field_witness(
        |value: &crate::testing::conformance::FenceIntegrityHandles| {
            let _ = &value.injector;
        },
    );
    // FIG-2105-TEST-WITNESS-0004: lash::testing::conformance::FenceIntegrityHandles::runtime [field]
    field_witness(
        |value: &crate::testing::conformance::FenceIntegrityHandles| {
            let _ = &value.runtime;
        },
    );
    // FIG-2105-TEST-WITNESS-0005: lash::testing::conformance::FenceIntegrityHandles::triggers [field]
    field_witness(
        |value: &crate::testing::conformance::FenceIntegrityHandles| {
            let _ = &value.triggers;
        },
    );
    // FIG-2105-TEST-WITNESS-0006: lash::testing::conformance::FenceIntegrityInjector [trait]
    fn trait_witness_0006<T: crate::testing::conformance::FenceIntegrityInjector>() {}
    // FIG-2105-TEST-WITNESS-0007: lash::testing::conformance::FenceIntegrityInjector::inject_raw_value [function]
    fn method_witness_0007<T: crate::testing::conformance::FenceIntegrityInjector>(
        value: &T,
        target: &crate::testing::conformance::FenceIntegrityTarget,
    ) {
        std::mem::drop(
            crate::testing::conformance::FenceIntegrityInjector::inject_raw_value(value, target, 0),
        );
    }
    // FIG-2105-TEST-WITNESS-0008: lash::testing::conformance::FenceIntegrityInjector::observe_raw_value [function]
    fn method_witness_0008<T: crate::testing::conformance::FenceIntegrityInjector>(
        value: &T,
        target: &crate::testing::conformance::FenceIntegrityTarget,
    ) {
        std::mem::drop(
            crate::testing::conformance::FenceIntegrityInjector::observe_raw_value(value, target),
        );
    }
    // FIG-2105-TEST-WITNESS-0009: lash::testing::conformance::FenceIntegrityObservation [struct]
    type_witness::<crate::testing::conformance::FenceIntegrityObservation>();
    // FIG-2105-TEST-WITNESS-0010: lash::testing::conformance::FenceIntegrityObservation::mutation_fingerprint [field]
    field_witness(
        |value: &crate::testing::conformance::FenceIntegrityObservation| {
            let _ = &value.mutation_fingerprint;
        },
    );
    // FIG-2105-TEST-WITNESS-0011: lash::testing::conformance::FenceIntegrityObservation::value [field]
    field_witness(
        |value: &crate::testing::conformance::FenceIntegrityObservation| {
            let crate::testing::conformance::FenceIntegrityObservation { value, .. } = value;
            let _ = value;
        },
    );
    // FIG-2105-TEST-WITNESS-0012: lash::testing::conformance::FenceIntegrityTarget [enum]
    type_witness::<crate::testing::conformance::FenceIntegrityTarget>();
    // FIG-2105-TEST-WITNESS-0013: lash::testing::conformance::FenceIntegrityTarget::QueuedWorkClaimFence [variant]
    variant_witness(
        |value: &crate::testing::conformance::FenceIntegrityTarget| {
            matches!(
                value,
                crate::testing::conformance::FenceIntegrityTarget::QueuedWorkClaimFence { .. }
            )
        },
    );
    // FIG-2105-TEST-WITNESS-0014: lash::testing::conformance::FenceIntegrityTarget::QueuedWorkClaimFence::batch_id [field]
    field_witness(
        |value: &crate::testing::conformance::FenceIntegrityTarget| {
            if let crate::testing::conformance::FenceIntegrityTarget::QueuedWorkClaimFence {
                batch_id,
                ..
            } = value
            {
                let _ = batch_id;
            }
        },
    );
    // FIG-2105-TEST-WITNESS-0015: lash::testing::conformance::FenceIntegrityTarget::SessionHeadRevision [variant]
    variant_witness(
        |value: &crate::testing::conformance::FenceIntegrityTarget| {
            matches!(
                value,
                crate::testing::conformance::FenceIntegrityTarget::SessionHeadRevision { .. }
            )
        },
    );
    // FIG-2105-TEST-WITNESS-0016: lash::testing::conformance::FenceIntegrityTarget::SessionHeadRevision::session_id [field]
    field_witness(
        |value: &crate::testing::conformance::FenceIntegrityTarget| {
            if let crate::testing::conformance::FenceIntegrityTarget::SessionHeadRevision {
                session_id,
                ..
            } = value
            {
                let _ = session_id;
            }
        },
    );
    // FIG-2105-TEST-WITNESS-0017: lash::testing::conformance::FenceIntegrityTarget::SessionLeaseFencingToken [variant]
    variant_witness(
        |value: &crate::testing::conformance::FenceIntegrityTarget| {
            matches!(
                value,
                crate::testing::conformance::FenceIntegrityTarget::SessionLeaseFencingToken { .. }
            )
        },
    );
    // FIG-2105-TEST-WITNESS-0018: lash::testing::conformance::FenceIntegrityTarget::SessionLeaseFencingToken::session_id [field]
    field_witness(
        |value: &crate::testing::conformance::FenceIntegrityTarget| {
            if let crate::testing::conformance::FenceIntegrityTarget::SessionLeaseFencingToken {
                session_id,
                ..
            } = value
            {
                let _ = session_id;
            }
        },
    );
    // FIG-2105-TEST-WITNESS-0019: lash::testing::conformance::FenceIntegrityTarget::TriggerRevision [variant]
    variant_witness(
        |value: &crate::testing::conformance::FenceIntegrityTarget| {
            matches!(
                value,
                crate::testing::conformance::FenceIntegrityTarget::TriggerRevision { .. }
            )
        },
    );
    // FIG-2105-TEST-WITNESS-0020: lash::testing::conformance::FenceIntegrityTarget::TriggerRevision::subscription_id [field]
    field_witness(
        |value: &crate::testing::conformance::FenceIntegrityTarget| {
            if let crate::testing::conformance::FenceIntegrityTarget::TriggerRevision {
                subscription_id,
                ..
            } = value
            {
                let _ = subscription_id;
            }
        },
    );
    // FIG-2105-TEST-WITNESS-0021: lash::testing::conformance::SessionExecutionLeaseRenewalZeroRowHandles [struct]
    type_witness::<crate::testing::conformance::SessionExecutionLeaseRenewalZeroRowHandles>();
    // FIG-2105-TEST-WITNESS-0022: lash::testing::conformance::SessionExecutionLeaseRenewalZeroRowHandles::injector [field]
    field_witness(
        |value: &crate::testing::conformance::SessionExecutionLeaseRenewalZeroRowHandles| {
            let _ = &value.injector;
        },
    );
    // FIG-2105-TEST-WITNESS-0023: lash::testing::conformance::SessionExecutionLeaseRenewalZeroRowHandles::store [field]
    field_witness(
        |value: &crate::testing::conformance::SessionExecutionLeaseRenewalZeroRowHandles| {
            let _ = &value.store;
        },
    );
    // FIG-2105-TEST-WITNESS-0024: lash::testing::conformance::SessionExecutionLeaseRenewalZeroRowInjector [trait]
    fn trait_witness_0024<
        T: crate::testing::conformance::SessionExecutionLeaseRenewalZeroRowInjector,
    >() {
    }
    // FIG-2105-TEST-WITNESS-0025: lash::testing::conformance::SessionExecutionLeaseRenewalZeroRowInjector::arm [function]
    fn method_witness_0025<
        T: crate::testing::conformance::SessionExecutionLeaseRenewalZeroRowInjector,
    >(
        value: &T,
    ) {
        std::mem::drop(
            crate::testing::conformance::SessionExecutionLeaseRenewalZeroRowInjector::arm(
                value, "session",
            ),
        );
    }
    // FIG-2105-TEST-WITNESS-0026: lash::testing::conformance::SessionExecutionLeaseRenewalZeroRowInjector::disarm [function]
    fn method_witness_0026<
        T: crate::testing::conformance::SessionExecutionLeaseRenewalZeroRowInjector,
    >(
        value: &T,
    ) {
        std::mem::drop(
            crate::testing::conformance::SessionExecutionLeaseRenewalZeroRowInjector::disarm(value),
        );
    }
    // FIG-2105-TEST-WITNESS-0027: lash::testing::conformance::UnboundSessionAdmissionState [enum]
    type_witness::<crate::testing::conformance::UnboundSessionAdmissionState>();
    // FIG-2105-TEST-WITNESS-0028: lash::testing::conformance::UnboundSessionAdmissionState::AdmittedOnly [variant]
    variant_witness(
        |value: &crate::testing::conformance::UnboundSessionAdmissionState| {
            matches!(
                value,
                crate::testing::conformance::UnboundSessionAdmissionState::AdmittedOnly
            )
        },
    );
    // FIG-2105-TEST-WITNESS-0029: lash::testing::conformance::UnboundSessionAdmissionState::Committed [variant]
    variant_witness(
        |value: &crate::testing::conformance::UnboundSessionAdmissionState| {
            matches!(
                value,
                crate::testing::conformance::UnboundSessionAdmissionState::Committed
            )
        },
    );
    // FIG-2105-TEST-WITNESS-0030: lash::testing::conformance::UnboundSessionResolutionHandles [struct]
    type_witness::<crate::testing::conformance::UnboundSessionResolutionHandles>();
    // FIG-2105-TEST-WITNESS-0031: lash::testing::conformance::UnboundSessionResolutionHandles::backend_name [field]
    field_witness(
        |value: &crate::testing::conformance::UnboundSessionResolutionHandles| {
            let _ = &value.backend_name;
        },
    );
    // FIG-2105-TEST-WITNESS-0032: lash::testing::conformance::UnboundSessionResolutionHandles::factory [field]
    field_witness(
        |value: &crate::testing::conformance::UnboundSessionResolutionHandles| {
            let _ = &value.factory;
        },
    );
    // FIG-2105-TEST-WITNESS-0033: lash::testing::conformance::UnboundSessionResolutionHandles::open_unbound [field]
    field_witness(
        |value: &crate::testing::conformance::UnboundSessionResolutionHandles| {
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
    // FIG-2105-TEST-WITNESS-0036: lash_core::facade_support::SessionGraphFacadeOps::extend_node_records [function]
    fn method_witness_0036<T: crate::facade_support::SessionGraphFacadeOps>(value: &mut T) {
        crate::facade_support::SessionGraphFacadeOps::extend_node_records(
            value,
            Vec::<crate::SessionNodeRecord>::new(),
        );
    }
    // FIG-2105-TEST-WITNESS-0037: lash::testing::conformance::GraphIntegrityCorruption [enum]
    type_witness::<crate::testing::conformance::GraphIntegrityCorruption>();
    // FIG-2105-TEST-WITNESS-0038: lash::testing::conformance::GraphIntegrityCorruption::DanglingLeafId [variant]
    variant_witness(
        |value: &crate::testing::conformance::GraphIntegrityCorruption| {
            matches!(
                value,
                crate::testing::conformance::GraphIntegrityCorruption::DanglingLeafId
            )
        },
    );
    // FIG-2105-TEST-WITNESS-0039: lash::testing::conformance::GraphIntegrityCorruption::DuplicateNodeId [variant]
    variant_witness(
        |value: &crate::testing::conformance::GraphIntegrityCorruption| {
            matches!(
                value,
                crate::testing::conformance::GraphIntegrityCorruption::DuplicateNodeId
            )
        },
    );
    // FIG-2105-TEST-WITNESS-0040: lash::testing::conformance::GraphIntegrityCorruption::OrphanLeaf [variant]
    variant_witness(
        |value: &crate::testing::conformance::GraphIntegrityCorruption| {
            matches!(
                value,
                crate::testing::conformance::GraphIntegrityCorruption::OrphanLeaf
            )
        },
    );
    // FIG-2105-TEST-WITNESS-0041: lash::testing::conformance::GraphIntegrityCorruption::ParentCycle [variant]
    variant_witness(
        |value: &crate::testing::conformance::GraphIntegrityCorruption| {
            matches!(
                value,
                crate::testing::conformance::GraphIntegrityCorruption::ParentCycle
            )
        },
    );
    // FIG-2105-TEST-WITNESS-0042: lash::testing::conformance::GraphIntegrityHandles [struct]
    type_witness::<crate::testing::conformance::GraphIntegrityHandles>();
    // FIG-2105-TEST-WITNESS-0043: lash::testing::conformance::GraphIntegrityHandles::injector [field]
    field_witness(
        |value: &crate::testing::conformance::GraphIntegrityHandles| {
            let _ = &value.injector;
        },
    );
    // FIG-2105-TEST-WITNESS-0044: lash::testing::conformance::GraphIntegrityHandles::runtime [field]
    field_witness(
        |value: &crate::testing::conformance::GraphIntegrityHandles| {
            let _ = &value.runtime;
        },
    );
    // FIG-2105-TEST-WITNESS-0045: lash::testing::conformance::GraphIntegrityInjector [trait]
    fn trait_witness_0045<T: crate::testing::conformance::GraphIntegrityInjector>() {}
    // FIG-2105-TEST-WITNESS-0046: lash::testing::conformance::GraphIntegrityInjector::cleanup [function]
    fn method_witness_0046<T: crate::testing::conformance::GraphIntegrityInjector>(
        value: &T,
        target: &crate::testing::conformance::GraphIntegrityTarget,
    ) {
        std::mem::drop(crate::testing::conformance::GraphIntegrityInjector::cleanup(value, target));
    }
    // FIG-2105-TEST-WITNESS-0047: lash::testing::conformance::GraphIntegrityInjector::inject [function]
    fn method_witness_0047<T: crate::testing::conformance::GraphIntegrityInjector>(
        value: &T,
        target: &crate::testing::conformance::GraphIntegrityTarget,
    ) {
        std::mem::drop(crate::testing::conformance::GraphIntegrityInjector::inject(
            value, target,
        ));
    }
    // FIG-2105-TEST-WITNESS-0048: lash::testing::conformance::GraphIntegrityInjector::load_whole_graph [function]
    fn method_witness_0048<T: crate::testing::conformance::GraphIntegrityInjector>(value: &T) {
        std::mem::drop(
            crate::testing::conformance::GraphIntegrityInjector::load_whole_graph(value, "session"),
        );
    }
    // FIG-2105-TEST-WITNESS-0049: lash::testing::conformance::GraphIntegrityRead [enum]
    type_witness::<crate::testing::conformance::GraphIntegrityRead>();
    // FIG-2105-TEST-WITNESS-0050: lash::testing::conformance::GraphIntegrityRead::ActivePath [variant]
    variant_witness(|value: &crate::testing::conformance::GraphIntegrityRead| {
        matches!(
            value,
            crate::testing::conformance::GraphIntegrityRead::ActivePath
        )
    });
    // FIG-2105-TEST-WITNESS-0051: lash::testing::conformance::GraphIntegrityRead::WholeGraph [variant]
    variant_witness(|value: &crate::testing::conformance::GraphIntegrityRead| {
        matches!(
            value,
            crate::testing::conformance::GraphIntegrityRead::WholeGraph
        )
    });
    // FIG-2105-TEST-WITNESS-0052: lash::testing::conformance::GraphIntegrityTarget [struct]
    type_witness::<crate::testing::conformance::GraphIntegrityTarget>();
    // FIG-2105-TEST-WITNESS-0053: lash::testing::conformance::GraphIntegrityTarget::corruption [field]
    field_witness(
        |value: &crate::testing::conformance::GraphIntegrityTarget| {
            let _ = &value.corruption;
        },
    );
    // FIG-2105-TEST-WITNESS-0054: lash::testing::conformance::GraphIntegrityTarget::leaf_node_id [field]
    field_witness(
        |value: &crate::testing::conformance::GraphIntegrityTarget| {
            let _ = &value.leaf_node_id;
        },
    );
    // FIG-2105-TEST-WITNESS-0055: lash::testing::conformance::GraphIntegrityTarget::missing_node_id [field]
    field_witness(
        |value: &crate::testing::conformance::GraphIntegrityTarget| {
            let _ = &value.missing_node_id;
        },
    );
    // FIG-2105-TEST-WITNESS-0056: lash::testing::conformance::GraphIntegrityTarget::read [field]
    field_witness(
        |value: &crate::testing::conformance::GraphIntegrityTarget| {
            let _ = &value.read;
        },
    );
    // FIG-2105-TEST-WITNESS-0057: lash::testing::conformance::GraphIntegrityTarget::root_node_id [field]
    field_witness(
        |value: &crate::testing::conformance::GraphIntegrityTarget| {
            let _ = &value.root_node_id;
        },
    );
    // FIG-2105-TEST-WITNESS-0058: lash::testing::conformance::GraphIntegrityTarget::session_id [field]
    field_witness(
        |value: &crate::testing::conformance::GraphIntegrityTarget| {
            let _ = &value.session_id;
        },
    );
    // FIG-2105-TEST-WITNESS-0059: lash::testing::conformance::GraphFactObservation [struct]
    type_witness::<crate::testing::conformance::GraphFactObservation>();
    // FIG-2105-TEST-WITNESS-0060: lash::testing::conformance::GraphFactObservation::frame_node_id [field]
    field_witness(
        |value: &crate::testing::conformance::GraphFactObservation| {
            let _ = &value.frame_node_id;
        },
    );
    // FIG-2105-TEST-WITNESS-0061: lash::testing::conformance::GraphFactObservation::generation [field]
    field_witness(
        |value: &crate::testing::conformance::GraphFactObservation| {
            let _ = &value.generation;
        },
    );
    // FIG-2105-TEST-WITNESS-0062: lash::testing::conformance::GraphFactObservation::is_frame [field]
    field_witness(
        |value: &crate::testing::conformance::GraphFactObservation| {
            let _ = &value.is_frame;
        },
    );
    // FIG-2105-TEST-WITNESS-0063: lash::testing::conformance::GraphFactObservation::node_id [field]
    field_witness(
        |value: &crate::testing::conformance::GraphFactObservation| {
            let _ = &value.node_id;
        },
    );
    // FIG-2105-TEST-WITNESS-0064: lash::testing::conformance::GraphFactObservation::owning_session_id [field]
    field_witness(
        |value: &crate::testing::conformance::GraphFactObservation| {
            let _ = &value.owning_session_id;
        },
    );
    // FIG-2105-TEST-WITNESS-0065: lash::testing::conformance::GraphFactObservation::parent_node_id [field]
    field_witness(
        |value: &crate::testing::conformance::GraphFactObservation| {
            let _ = &value.parent_node_id;
        },
    );
    // FIG-2105-TEST-WITNESS-0066: lash::testing::conformance::LineageConformanceHandles [struct]
    type_witness::<crate::testing::conformance::LineageConformanceHandles>();
    // FIG-2105-TEST-WITNESS-0067: lash::testing::conformance::LineageConformanceHandles::factory [field]
    field_witness(
        |value: &crate::testing::conformance::LineageConformanceHandles| {
            let _ = &value.factory;
        },
    );
    // FIG-2105-TEST-WITNESS-0068: lash::testing::conformance::LineageConformanceHandles::injector [field]
    field_witness(
        |value: &crate::testing::conformance::LineageConformanceHandles| {
            let _ = &value.injector;
        },
    );
    // FIG-2105-TEST-WITNESS-0069: lash::testing::conformance::LineageConformanceInjector [trait]
    fn trait_witness_0069<T: crate::testing::conformance::LineageConformanceInjector>() {}
    // FIG-2105-TEST-WITNESS-0070: lash::testing::conformance::LineageConformanceInjector::all_graph_facts [function]
    fn method_witness_0070<T: crate::testing::conformance::LineageConformanceInjector>(value: &T) {
        std::mem::drop(
            crate::testing::conformance::LineageConformanceInjector::all_graph_facts(value),
        );
    }
    // FIG-2105-TEST-WITNESS-0071: lash::testing::conformance::LineageConformanceInjector::edge_path [function]
    fn method_witness_0071<T: crate::testing::conformance::LineageConformanceInjector>(value: &T) {
        std::mem::drop(
            crate::testing::conformance::LineageConformanceInjector::edge_path(value, "session"),
        );
    }
    // FIG-2105-TEST-WITNESS-0072: lash::testing::conformance::LineageConformanceInjector::force_lineage [function]
    fn method_witness_0072<T: crate::testing::conformance::LineageConformanceInjector>(value: &T) {
        std::mem::drop(
            crate::testing::conformance::LineageConformanceInjector::force_lineage(
                value, "session", "node",
            ),
        );
    }
    // FIG-2105-TEST-WITNESS-0073: lash::testing::conformance::LineageConformanceInjector::lineage_ancestors [function]
    fn method_witness_0073<T: crate::testing::conformance::LineageConformanceInjector>(value: &T) {
        std::mem::drop(
            crate::testing::conformance::LineageConformanceInjector::lineage_ancestors(
                value, "session",
            ),
        );
    }
    // FIG-2105-TEST-WITNESS-0074: lash::testing::conformance::LineageConformanceInjector::tombstone_node [function]
    fn method_witness_0074<T: crate::testing::conformance::LineageConformanceInjector>(value: &T) {
        std::mem::drop(
            crate::testing::conformance::LineageConformanceInjector::tombstone_node(value, "node"),
        );
    }
}
