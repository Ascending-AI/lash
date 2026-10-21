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
//! document or a process, and [`plan_carry`] plans a successor pair its host
//! names. Parked page rewriting and validation follow this document plan;
//! only the fenced owner transaction publishes the carried run.

use lash_kernel_doc::Document;
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::validate_document;
use lash_kernel_migrate::{DocumentRefusal, ParkedRefusal, Relink, Rewritten};
use lash_vocabulary::legacy_names::DocumentId;

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
    /// The code the adoption moves off or onto is not read from the
    /// store, or not admitted there (FIG-6985).
    #[error("the adopted code is not read: {message}")]
    Unread { message: String },
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
    OtherEntry {
        entry: lash_vocabulary::legacy_names::Name,
    },
}

/// A document adopted onto a successor release.
#[derive(Clone, Debug)]
pub struct AdoptedRun {
    pub from: DocumentId,
    pub rewritten: Rewritten,
}

/// `document`, a published definition's or a process's, adopted by
/// `relink`; `None` when it lists no function `relink` names. `functions` holds both releases.
///
/// # Errors
///
/// The typed refusal of a document or run that is not adopted.
pub fn plan_adoption(
    document: &Document,
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
    plan_carry(document, functions, relink).map(Some)
}

/// `document` carried onto `relink`: each library function it calls
/// replaced by its successor. `functions` holds every function the
/// document reaches and every successor. The paged carry applies this plan to a parked run.
///
/// # Errors
///
/// The typed refusal of a document or run that is not carried.
pub fn plan_carry(
    document: &Document,
    functions: &FunctionRegistry,
    relink: &Relink,
) -> Result<AdoptedRun, HelperAdoptionRefusal> {
    let from = document.identity();
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
