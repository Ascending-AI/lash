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
//! The trigger family lives in its own database, so a store set attaches it to
//! its registry's connection. A registry opened on its own holds no delivery a
//! start could name, and reads none.

use std::sync::LazyLock;

use lash_core_execution::runtime::TriggerDeliveryBinding;
use lash_store_sql::trigger::deliveries::DeliveryStatements;
use rusqlite::{OptionalExtension, params};

use crate::schema_layout::Schema;

static ATTACHED_DELIVERY_SQL: LazyLock<DeliveryStatements> =
    LazyLock::new(|| DeliveryStatements::render(Schema::TriggerStore.dialect()));

/// Whether a registry's connection reaches its store set's trigger
/// deliveries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TriggerDeliveryBindings {
    /// The store set's trigger store is attached to the connection.
    Attached,
    /// The registry was opened on its own: no trigger store shares its
    /// connection, and no delivery's binding is read.
    Detached,
}

impl TriggerDeliveryBindings {
    /// Admit the start `pin` names against its delivery's row, read on
    /// `conn` inside the caller's transaction once the start's key found
    /// nothing. A detached registry reads nothing and admits it.
    pub(crate) fn check_start_conn(
        self,
        conn: &rusqlite::Connection,
        pin: &lash_core_execution::TriggerDeliveryPin,
    ) -> Result<(), lash_core_execution::PluginError> {
        let Self::Attached = self else {
            return Ok(());
        };
        let row = conn
            .query_row(
                ATTACHED_DELIVERY_SQL.select_bound_process_id.sql(),
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
}
