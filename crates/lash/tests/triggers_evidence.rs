//! Compile-time witnesses for trigger-area facade and integrator contracts.
//!
//! FIG-2107 drains the ledger's remaining `unused-justify` slices: at the
//! dispatch-time recount this area held 40 rows. The 36 rows whose item
//! still exists are type-checked here through the path a host or integrator
//! would name — `lash::` for facade surface, `lash_core::` for internal seams
//! the integrator classes consume directly. The 4 rows whose item no longer
//! exists anywhere in this workspace are listed in the pull request rather
//! than witnessed here.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables, unused_imports)]
#![allow(clippy::all)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

fn drain_area_witnesses() {
    // W0001: lash::durability::DurableProcessWorkerConfig::trigger_store [field]
    field_witness(|value: &lash::durability::DurableProcessWorkerConfig| {
        let _ = &value.trigger_store;
    });
    // W0002: lash::durability::DurableProcessWorkerConfig::with_trigger_store [function]
    let _ = lash::durability::DurableProcessWorkerConfig::with_trigger_store;
    // W0003: lash::plugins::PluginSession::triggers [function]
    let _ = lash::plugins::PluginSession::triggers;
    // W0006: lash_conformance::TriggerOccurrenceRetentionFaultInjector::clear_occurrence_delete_failure [function]
    fn meth_0006<T: lash_conformance::TriggerOccurrenceRetentionFaultInjector>(_: &T) {
        let _ = T::clear_occurrence_delete_failure;
    }
    // W0007: lash_conformance::TriggerOccurrenceRetentionFaultInjector::fail_occurrence_delete [function]
    fn meth_0007<T: lash_conformance::TriggerOccurrenceRetentionFaultInjector>(_: &T) {
        let _ = T::fail_occurrence_delete;
    }
    // W0008: lash::tools::ToolContext::triggers [function]
    let _ = lash::tools::ToolContext::triggers;
    // W0009: lash::tools::ToolTriggerClient [struct]
    type_witness::<lash::tools::ToolTriggerClient>();
    // W0010: lash::tools::ToolTriggerClient::emit [function]
    let _ = lash::tools::ToolTriggerClient::emit;
    // W0015: lash::triggers::LashSchema::object [function]
    let _ = lash::triggers::LashSchema::object;
    // W0016: lash::triggers::LashSchema::schema [field]
    field_witness(|value: &lash::triggers::LashSchema| {
        let _ = &value.schema;
    });
    // W0017: lash::triggers::LashSchema::validate [function]
    let _ = lash::triggers::LashSchema::validate;
    // W0018: lash::triggers::TriggerCommand::Delete::actor [field]
    field_witness(|value: &lash::triggers::TriggerCommand| {
        if let lash::triggers::TriggerCommand::Delete { actor, .. } = value {
            let _ = actor;
        }
    });
    // W0019: lash::triggers::TriggerCommand::Disable::actor [field]
    field_witness(|value: &lash::triggers::TriggerCommand| {
        if let lash::triggers::TriggerCommand::Disable { actor, .. } = value {
            let _ = actor;
        }
    });
    // W0020: lash::triggers::TriggerCommand::Enable::actor [field]
    field_witness(|value: &lash::triggers::TriggerCommand| {
        if let lash::triggers::TriggerCommand::Enable { actor, .. } = value {
            let _ = actor;
        }
    });
    // W0021: lash::triggers::TriggerDeliveryEmitOutcome [enum]
    type_witness::<lash::triggers::TriggerDeliveryEmitOutcome>();
    // W0022: lash::triggers::TriggerDeliveryEmitOutcome::AlreadyReserved [variant]
    variant_witness(|value: &lash::triggers::TriggerDeliveryEmitOutcome| {
        matches!(
            value,
            lash::triggers::TriggerDeliveryEmitOutcome::AlreadyReserved
        )
    });
    // W0023: lash::triggers::TriggerDeliveryEmitOutcome::Failed [variant]
    variant_witness(|value: &lash::triggers::TriggerDeliveryEmitOutcome| {
        matches!(
            value,
            lash::triggers::TriggerDeliveryEmitOutcome::Failed { .. }
        )
    });
    // W0024: lash::triggers::TriggerDeliveryEmitOutcome::Failed::reason [field]
    field_witness(|value: &lash::triggers::TriggerDeliveryEmitOutcome| {
        if let lash::triggers::TriggerDeliveryEmitOutcome::Failed { reason, .. } = value {
            let _ = reason;
        }
    });
    // W0025: lash::triggers::TriggerDeliveryEmitOutcome::Started [variant]
    variant_witness(|value: &lash::triggers::TriggerDeliveryEmitOutcome| {
        matches!(value, lash::triggers::TriggerDeliveryEmitOutcome::Started)
    });
    // W0026: lash::triggers::TriggerDeliveryEmitReceipt [struct]
    type_witness::<lash::triggers::TriggerDeliveryEmitReceipt>();
    // W0027: lash::triggers::TriggerDeliveryEmitReceipt::occurrence_id [field]
    field_witness(|value: &lash::triggers::TriggerDeliveryEmitReceipt| {
        let _ = &value.occurrence_id;
    });
    // W0028: lash::triggers::TriggerDeliveryEmitReceipt::outcome [field]
    field_witness(|value: &lash::triggers::TriggerDeliveryEmitReceipt| {
        let _ = &value.outcome;
    });
    // W0029: lash::triggers::TriggerDeliveryEmitReceipt::process_id [field]
    field_witness(|value: &lash::triggers::TriggerDeliveryEmitReceipt| {
        let _ = &value.process_id;
    });
    // W0030: lash::triggers::TriggerDeliveryEmitReceipt::subscription_id [field]
    field_witness(|value: &lash::triggers::TriggerDeliveryEmitReceipt| {
        let _ = &value.subscription_id;
    });
    // W0031: lash::triggers::TriggerEmitReport::deliveries [field]
    field_witness(|value: &lash::triggers::TriggerEmitReport| {
        let _ = &value.deliveries;
    });
    // W0032: lash::triggers::TriggerEmitReport::empty [function]
    let _ = lash::triggers::TriggerEmitReport::empty;
    // W0033: lash::triggers::TriggerEmitReport::occurrence_id [field]
    field_witness(|value: &lash::triggers::TriggerEmitReport| {
        let _ = &value.occurrence_id;
    });
    // W0034: lash::plugins::ProtocolBuildInput::trigger_events [field]
    field_witness(|value: &lash::plugins::ProtocolBuildInput| {
        let _ = &value.trigger_events;
    });
    // W0035: lash::plugins::RuntimeExecutionContext::execute_trigger_effect [function]
    let _ = lash::plugins::RuntimeExecutionContext::execute_trigger_effect;
    // W0036: lash::plugins::RuntimeExecutionContext::trigger_actor [function]
    let _ = lash::plugins::RuntimeExecutionContext::trigger_actor;
    // W0037: lash::plugins::RuntimeExecutionContext::trigger_owner_scope [function]
    let _ = lash::plugins::RuntimeExecutionContext::trigger_owner_scope;
    // W0038: lash::plugins::RuntimeExecutionContext::trigger_registration_wake_target [function]
    let _ = lash::plugins::RuntimeExecutionContext::trigger_registration_wake_target;
    // W0039: lash::plugins::RuntimeExecutionContext::trigger_store [function]
    let _ = lash::plugins::RuntimeExecutionContext::trigger_store;
    // W0040: lash::triggers::TriggerEventCatalog [struct]
    type_witness::<lash::triggers::TriggerEventCatalog>();
    // W0001: lash::persistence::AppendRequestIdentity::Append [variant]
    variant_witness(|value: &lash::persistence::AppendRequestIdentity| {
        matches!(
            value,
            lash::persistence::AppendRequestIdentity::Append { .. }
        )
    });
    // W0002: lash::persistence::AppendRequestIdentity::PlainCommit [variant]
    variant_witness(|value: &lash::persistence::AppendRequestIdentity| {
        matches!(value, lash::persistence::AppendRequestIdentity::PlainCommit)
    });
    // W0003: lash::persistence::StoreSchemaVerdict::Migratable [variant]
    variant_witness(|value: &lash::persistence::StoreSchemaVerdict| {
        matches!(
            value,
            lash::persistence::StoreSchemaVerdict::Migratable { .. }
        )
    });
    // W0001: lash_core::facade_support::visible_response_text_from_parts [function]
    let _ = lash_core::facade_support::visible_response_text_from_parts;
}
