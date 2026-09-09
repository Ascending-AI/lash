use super::StoreError;

/// Merges one durable token-ledger row without partially mutating the ledger
/// when a counter or the canonical non-reasoning total overflows.
pub fn merge_token_ledger_entry_checked(
    ledger: &mut Vec<crate::TokenLedgerEntry>,
    entry: crate::TokenLedgerEntry,
) -> Result<(), StoreError> {
    if entry.usage_disposition.row_is_empty(&entry.usage) {
        return Ok(());
    }
    entry
        .usage
        .checked_total()
        .map_err(|overflow| StoreError::TokenUsageAccountingOverflow {
            usage_source: entry.source.clone(),
            model: entry.model.clone(),
            counter: overflow.counter(),
        })?;
    let Some(existing) = ledger.iter_mut().find(|existing| {
        existing.source == entry.source
            && existing.model == entry.model
            && existing
                .usage_disposition
                .accumulates_with(&entry.usage_disposition)
    }) else {
        ledger.push(entry);
        return Ok(());
    };
    let merged = existing
        .usage
        .checked_add(&entry.usage)
        .map_err(|overflow| StoreError::TokenUsageAccountingOverflow {
            usage_source: entry.source.clone(),
            model: entry.model.clone(),
            counter: overflow.counter(),
        })?;
    if existing
        .usage_disposition
        .absorb_saturating(&entry.usage_disposition)
    {
        return Err(StoreError::TokenUsageAccountingOverflow {
            usage_source: entry.source,
            model: entry.model,
            counter: "unreported_attempts",
        });
    }
    existing.usage = merged;
    Ok(())
}

/// Folds durable token-ledger rows through the same checked merge used by the
/// runtime's staging and final-commit paths.
pub fn merge_token_ledger_entries_checked(
    entries: Vec<crate::TokenLedgerEntry>,
) -> Result<Vec<crate::TokenLedgerEntry>, StoreError> {
    let mut merged = Vec::new();
    for entry in entries {
        merge_token_ledger_entry_checked(&mut merged, entry)?;
    }
    Ok(merged)
}
