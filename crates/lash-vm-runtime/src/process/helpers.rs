//! Kernel processes and published definitions adopted onto a successor
//! release of the packages they call (`K-LIB-009`).
//!
//! A package fix is a new function, and so is every function that calls
//! it: a document written against an earlier release pins that release's
//! functions by identity, and runs on exactly them while they are held.
//! Nothing moves it off them on its own. Adoption relinks it by name
//! ([`lash_kernel_migrate::relink`]): each function its code calls that the
//! earlier release holds is replaced by the successor's function of the
//! same name, unless that moves it onto a changed operation contract
//! (ADOPT-CONTRACT), which refuses naming the operation.
//!
//! [`plan_adoption`] plans that relink for a published definition's
//! document or a process that is not parked, and [`plan_carry`] carries a
//! process onto a successor pair its host names (`Processes::succeed`,
//! FIG-6121). A parked run is never adopted: its heap pages are carried by
//! no migrator this build ships (FIG-6233), so it is refused, typed
//! ([`ParkedRefusal::StateNotCarried`]), and stays parked on the release it
//! pins, with its rows intact.

use lash_kernel_doc::{Document, DocumentId, FunctionRegistry, validate_document};
use lash_kernel_migrate::{DocumentRefusal, ParkedRefusal, Relink, Rewritten};

/// Why a document or a run is not adopted onto other library functions:
/// a successor release's, or those of the document a successor pair names.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "refused")]
#[non_exhaustive]
pub enum HelperAdoptionRefusal {
    #[error("document {document} is not retained")]
    DocumentMissing { document: DocumentId },
    /// The relink refused the document: a function it calls has no
    /// successor or is not held, or the relink would change the contract of
    /// an operation it performs.
    #[error("document {document} is not adopted: {refusal}")]
    Document {
        document: DocumentId,
        refusal: DocumentRefusal,
    },
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
    /// The successor's document is not the process's document relinked
    /// onto the successor's functions of the same names: no correspondence
    /// carries the run onto it.
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

/// A document adopted onto a successor release.
#[derive(Clone, Debug)]
pub struct AdoptedRun {
    pub from: DocumentId,
    pub rewritten: Rewritten,
}

/// `document`, a published definition's or a process's, adopted by
/// `relink`; `None` when it lists no function `relink` names. With a run
/// `parked` under it, the adoption is refused: no migrator carries a
/// parked run's pages, so it stays on the release it pins. `functions`
/// holds both releases.
///
/// # Errors
///
/// The typed refusal of a document or run that is not adopted.
pub fn plan_adoption(
    document: &Document,
    parked: bool,
    functions: &FunctionRegistry,
    relink: &Relink,
) -> Result<Option<AdoptedRun>, HelperAdoptionRefusal> {
    // A document lists every function it reaches, through bodies too.
    if !document
        .manifest
        .functions
        .keys()
        .any(|function| relink.contains_key(function))
    {
        return Ok(None);
    }
    plan_carry(document, parked, functions, relink).map(Some)
}

/// `document` carried onto `relink`: each library function it calls
/// replaced by its successor. `functions` holds every function the
/// document reaches and every successor. A run `parked` under the document
/// is refused, typed: its pages stay as they are, and it resumes only on a
/// node that holds what it pins.
///
/// # Errors
///
/// The typed refusal of a document or run that is not carried.
pub fn plan_carry(
    document: &Document,
    parked: bool,
    functions: &FunctionRegistry,
    relink: &Relink,
) -> Result<AdoptedRun, HelperAdoptionRefusal> {
    let from = document.identity();
    if parked {
        return Err(HelperAdoptionRefusal::Parked {
            document: from,
            refusal: parked_not_carried(),
        });
    }
    let rewritten = lash_kernel_migrate::adopt(document, relink, functions).map_err(|refusal| {
        HelperAdoptionRefusal::Document {
            document: from,
            refusal,
        }
    })?;
    validate_document(&rewritten.document, functions).map_err(|invalid| {
        HelperAdoptionRefusal::NotAdmitted {
            document: from,
            reason: invalid.to_string(),
        }
    })?;
    Ok(AdoptedRun { from, rewritten })
}

/// Why a parked run is not carried onto another document: no migrator
/// this build ships carries its heap pages (FIG-6233).
pub(crate) fn parked_not_carried() -> ParkedRefusal {
    ParkedRefusal::StateNotCarried {
        detail: "a parked run's heap pages are carried by no migrator this build ships \
                 (FIG-6233); it stays parked and resumes on a node that holds what it pins"
            .to_owned(),
    }
}
