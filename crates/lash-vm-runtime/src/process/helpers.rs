//! The carry of a kernel process onto a successor pair whose document
//! names other library functions (FIG-6121): [`plan_carry`] replaces each
//! library function a document lists with the function the successor names
//! under the same name and rewrites the document
//! (`lash_kernel_migrate::adopt`). A parked run is never carried: its heap
//! pages are carried by no migrator this build ships, so it is refused,
//! typed ([`ParkedRefusal::StateNotCarried`]), stays parked with its rows
//! intact, and resumes on a node that holds what it pins (FIG-6193).

use std::collections::BTreeMap;

use lash_kernel_doc::{
    Document, DocumentId, FunctionId, FunctionName, FunctionRegistry, validate_document,
};
use lash_kernel_migrate::{ParkedRefusal, Rewritten};

/// Why a run is not carried onto other library functions: the build's own
/// helpers, or the document a successor pair names.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "refused")]
#[non_exhaustive]
pub enum HelperAdoptionRefusal {
    /// The document reaches `function`, and the build has no function of
    /// its name to adopt.
    #[error("helper `{name}` ({function}) has no counterpart in this build")]
    NoCounterpart {
        function: FunctionId,
        name: FunctionName,
    },
    #[error("document {document} is not retained")]
    DocumentMissing { document: DocumentId },
    /// The adopted document is not one this build runs.
    #[error("document {document}, adopted, is not admitted: {reason}")]
    NotAdmitted {
        document: DocumentId,
        reason: String,
    },
    #[error("the run parked under document {document} is not carried: {refusal}")]
    Parked {
        document: DocumentId,
        refusal: ParkedRefusal,
    },
    #[error("the process's stored state does not decode: {message}")]
    State { message: String },
    /// The successor's document is not the process's document with each
    /// library function it lists replaced by the successor's function of
    /// the same name: no correspondence carries the run onto it.
    #[error("no correspondence carries document {document} onto {successor}")]
    NoCorrespondence {
        document: DocumentId,
        successor: DocumentId,
    },
    /// The successor's payload starts another entry, or with other
    /// arguments: it is no successor of this run.
    #[error("the successor enters `{entry}` with other arguments than the run it would carry")]
    OtherEntry { entry: lash_kernel_doc::Name },
}

/// A document adopted onto a build's helpers.
#[derive(Clone, Debug)]
pub struct AdoptedRun {
    pub from: DocumentId,
    pub rewritten: Rewritten,
}

/// `document` carried onto `replacements`: each library function it names
/// replaced by its counterpart. `functions` holds every function the
/// document reaches and every replacement. A run `parked` under the
/// document is refused, typed: its pages stay as they are, and it resumes
/// only on a build that retains what it pins.
///
/// # Errors
///
/// The typed refusal of a document or run that is not carried.
pub fn plan_carry(
    document: &Document,
    parked: bool,
    functions: &FunctionRegistry,
    replacements: &BTreeMap<FunctionId, FunctionId>,
) -> Result<AdoptedRun, HelperAdoptionRefusal> {
    let state = |message: String| HelperAdoptionRefusal::State { message };
    let from = document.identity();
    if parked {
        return Err(HelperAdoptionRefusal::Parked {
            document: from,
            refusal: parked_not_carried(),
        });
    }
    let rewritten = lash_kernel_migrate::adopt(document, replacements, functions, &BTreeMap::new())
        .map_err(|refusal| state(refusal.to_string()))?;
    validate_document(&rewritten.document, functions).map_err(|invalid| {
        HelperAdoptionRefusal::NotAdmitted {
            document: from,
            reason: invalid.to_string(),
        }
    })?;
    Ok(AdoptedRun { from, rewritten })
}

/// Why a parked run is not carried onto another document: no migrator
/// this build ships carries its heap pages (FIG-6193).
pub(crate) fn parked_not_carried() -> ParkedRefusal {
    ParkedRefusal::StateNotCarried {
        detail: "a parked run's heap pages are carried by no migrator this build ships; \
                 it stays parked and resumes on a node that holds what it pins"
            .to_owned(),
    }
}
