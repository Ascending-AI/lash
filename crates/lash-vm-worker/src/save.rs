//! A session cell's end, saving the functions its carry would drop
//! (`K-SES-004`), in the worker that runs the cell: the dialect's rules
//! over the cell's sealed heap, each saved function described for the
//! session as lowering reads it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_dialect::{EffectControl, NotSaved};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::sites::Site;
use lash_kernel_vm::{CellSave, SaveCell, SaveRules, SavedCell};
use lash_vm_client::service::LowerFunction;

/// How a cell of one dialect saves its functions.
pub(crate) struct DialectSave {
    /// The cell's document and the Link it runs under.
    pub(crate) document: Arc<lash_kernel_doc::Document>,
    pub(crate) link: LinkId,
    /// Every other document the cell's code reaches, by its Link.
    pub(crate) documents: BTreeMap<LinkId, lash_kernel_doc::Document>,
    /// The library the cell runs against.
    pub(crate) catalog: Arc<lash_kernel_doc::FunctionRegistry>,
    /// The dialect's function values; none keeps no function.
    pub(crate) values: Option<lash_kernel_doc::FunctionRepresentation>,
    pub(crate) controls: BTreeMap<lash_kernel_value::names::OperationUse, BTreeSet<EffectControl>>,
    /// What the cell's source wrote of each function its document makes,
    /// by the site of its closure.
    pub(crate) written: BTreeMap<Site, lash_kernel_dialect::Written>,
}

impl CellSave for DialectSave {
    fn save(&mut self, cell: SaveCell<'_>) -> Result<SavedCell, HeapFault> {
        let Some(values) = &self.values else {
            return Ok(SavedCell {
                refused: cell
                    .dropped()
                    .iter()
                    .filter(|name| cell.binds(name))
                    .map(|name| (name.clone(), NotSaved::Dialect.to_string()))
                    .collect(),
                ..SavedCell::default()
            });
        };
        let saved = cell.save(SaveRules {
            document: &self.document,
            link: self.link,
            documents: &self.documents,
            controls: &self.controls,
            written: &self.written,
            values,
            catalog: self.catalog.as_ref(),
        })?;
        Ok(SavedCell {
            functions: saved
                .functions
                .into_iter()
                .map(|function| {
                    let description = LowerFunction::from(&function).description();
                    (function, description)
                })
                .collect(),
            carried: saved.carried.into_iter().collect(),
            refused: saved
                .refused
                .into_iter()
                .map(|(name, why)| (name, why.to_string()))
                .collect(),
        })
    }
}
