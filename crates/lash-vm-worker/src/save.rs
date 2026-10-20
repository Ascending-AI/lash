//! A session cell's end, saving the functions its carry would drop
//! (`K-SES-004`), in the worker that runs the cell: the dialect's own
//! rules decide which qualify, each one's captures are frozen once on the
//! cell's heap, and every binding that held one is bound to its value.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{EffectControl, Kept, Left, NotSaved};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::{EffectName, Site};
use lash_kernel_vm::{CellSave, CellSaveContext, SaveCell, SavedCell};
use lash_vm_client::service::LowerFunction;

/// How a cell of one dialect saves its functions.
pub(crate) struct DialectSave {
    /// The dialect's function values; none keeps no function.
    pub(crate) values: Option<lash_kernel_doc::FunctionRepresentation>,
    pub(crate) controls: BTreeMap<EffectName, BTreeSet<EffectControl>>,
    /// What the cell's source wrote of each function its document makes,
    /// by the site of its closure.
    pub(crate) written: BTreeMap<Site, lash_kernel_dialect::Written>,
}

impl CellSave for DialectSave {
    fn save(
        &mut self,
        saving: &mut CellSaveContext<'_>,
        cell: SaveCell<'_>,
    ) -> Result<SavedCell, HeapFault> {
        let Some(values) = &self.values else {
            return Ok(SavedCell {
                refused: cell
                    .dropped
                    .iter()
                    .filter(|name| cell.bindings.contains_key(*name))
                    .map(|name| (name.clone(), NotSaved::Dialect.to_string()))
                    .collect(),
                ..SavedCell::default()
            });
        };
        let (kept, refused) = saving.save(
            Left {
                document: cell.document,
                bindings: cell.bindings,
                not_carried: cell.dropped,
                controls: &self.controls,
                written: &self.written,
                values,
                closure: cell.closure,
                catalog: cell.catalog,
            },
            cell.held,
        )?;
        let mut saved = SavedCell {
            refused: refused
                .into_iter()
                .map(|(name, why)| (name, why.to_string()))
                .collect(),
            ..SavedCell::default()
        };
        for (name, Kept { function, aliases }) in kept {
            let description = LowerFunction::from(&function).description();
            let (record, value) = saving.record(&function, &description)?;
            // Every alias is the one function: one value, bound to each.
            for binding in std::iter::once(&name).chain(&aliases) {
                saved.rebound.push((binding.clone(), value.clone()));
            }
            saved.functions.push((function.name.clone(), record));
        }
        Ok(saved)
    }
}
