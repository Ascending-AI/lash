//! Field and index reads whose source is a projected host value.
//!
//! Split from the dialect read helpers because the question here is a different
//! one: not *what does this dialect call this property*, but *who answers the
//! read*. A custom projection is a lazy host view and the descriptor answers it;
//! a scalar projection is a value already in memory behind a handle, so it reads
//! through the dialect exactly as the same value does unprojected. Deciding that
//! by projection kind, in one place, is what keeps `text.length` from depending
//! on whether `text` came from the host (FIG-1482).

use super::*;

impl<H: ExecutionHost> Vm<'_, H> {
    /// Field access on a projected source.
    ///
    /// `ProjectedValue::get_field` can only fall back to the dialect-blind
    /// `access.rs` read, which reports `.length` on a projected string as
    /// unreadable and answers `null` where the TypeScript dialect says
    /// `undefined`. A scalar projection therefore reads through
    /// `read_dialect_field` instead; a custom one still asks its descriptor, so
    /// nothing is dragged across to serve one property.
    ///
    /// The result keeps the projected wrapper either way, so a path expression
    /// still carries "this came from a projected source".
    pub(super) async fn read_projected_field(
        &mut self,
        projected: &ProjectedValue,
        field: &Name,
    ) -> Result<Value, RuntimeError> {
        let inner = match projected.scalar_value() {
            Some(value) => self.read_dialect_field(value.clone(), field)?,
            None => projected.get_field(field).await?,
        };
        Ok(ProjectedValue::propagate_field(
            projected.name(),
            &field.text,
            inner,
        ))
    }

    /// Index access on a projected source, split by projection kind for the same
    /// reason as `read_projected_field`: a scalar projection indexes exactly as
    /// the value behind it does, which for the TypeScript dialect is UTF-16 units
    /// and `undefined` for an absent key.
    pub(super) async fn read_projected_index(
        &mut self,
        projected: &ProjectedValue,
        index: &Value,
    ) -> Result<Value, RuntimeError> {
        let inner = match projected.scalar_value() {
            Some(value) => self.read_dialect_index(value.clone(), index.clone())?,
            None => projected.get_index(index).await?,
        };
        Ok(ProjectedValue::propagate_index(
            projected.name(),
            index,
            inner,
        ))
    }
}
