//! The trigger-delivery binding a delivery's start is registered against
//! (ADR 0107 §5, FIG-4369).
//!
//! A delivery's start is keyed by the delivery, and its key finds the process
//! it minted only while that process is retained. Once the delivery is bound
//! and its process pruned, a start that ingested the delivery before the bind
//! would find nothing under the key and mint a second process. The registrar
//! therefore reads the delivery's binding in the transaction that checks the
//! start key. A bound delivery, or one retention has since removed, registers
//! nothing.
//!
//! The trigger family lives in the deployment's one database, so the
//! registrar reads the binding on its own connection.

use std::sync::LazyLock;

use lash_core_execution::runtime::TriggerDeliveryBinding;
use lash_store_sql::trigger::deliveries::DeliveryStatements;
use rusqlite::{OptionalExtension, params};

static DELIVERY_SQL: LazyLock<DeliveryStatements> =
    LazyLock::new(|| DeliveryStatements::render(crate::schema_layout::MAIN));

/// Admit the start `pin` names against its delivery's row, read on `conn`
/// inside the caller's transaction once the start's key found nothing.
pub(crate) fn check_start_conn(
    conn: &rusqlite::Connection,
    pin: &lash_core_execution::TriggerDeliveryPin,
) -> Result<(), lash_core_execution::PluginError> {
    let row = conn
        .query_row(
            DELIVERY_SQL.select_bound_process_id.sql(),
            params![pin.occurrence_id.as_str(), pin.subscription_id.as_str()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(crate::process_sqlite_error)?;
    let binding = match row {
        None => TriggerDeliveryBinding::Absent,
        Some(None) => TriggerDeliveryBinding::Unbound,
        Some(Some(process_id)) => {
            TriggerDeliveryBinding::Bound(crate::codec::stored_process_id(&process_id)?)
        }
    };
    lash_core_execution::runtime::check_trigger_delivery_start(pin, binding)
}
