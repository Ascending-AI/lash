//! A session cell's end, saving the functions its carry would drop
//! (`K-SES-004`), in the worker that runs the cell: every closure a
//! dropped binding reaches is saved once, its captures frozen on the
//! cell's heap, and each of those bindings is bound to its value with
//! every closure in it a reference to its saved function.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{EffectControl, Left, NotSaved};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_vm::{CellSave, CellSaveContext, SaveCell, SavedCell};
use lash_vm_client::service::LowerFunction;

use lash_vocabulary::sites::Site;

/// How a cell of one dialect saves its functions.
pub(crate) struct DialectSave {
    pub(crate) saved_written:
        BTreeMap<lash_kernel_doc::heap::EntityId, lash_kernel_dialect::Written>,
    /// The dialect's function values; none keeps no function.
    pub(crate) values: Option<lash_kernel_doc::FunctionRepresentation>,
    pub(crate) controls: BTreeMap<lash_vocabulary::names::OperationUse, BTreeSet<EffectControl>>,
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
        let saved = saving.save(
            Left {
                link: cell.link,
                documents: cell.documents,
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
        let mut cell = SavedCell {
            refused: saved
                .refused
                .into_iter()
                .map(|(name, why)| (name, why.to_string()))
                .collect(),
            rebound: saved.bindings.into_iter().collect(),
            ..SavedCell::default()
        };
        for (name, function) in saved.functions {
            if saving.already_recorded(&function)? {
                continue;
            }
            let description = LowerFunction::from(&function).description();
            cell.functions
                .push((name, saving.record(&function, &description)?));
        }
        Ok(cell)
    }
}
