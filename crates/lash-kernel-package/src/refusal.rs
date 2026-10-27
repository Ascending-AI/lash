use {
    lash_kernel_value::names::*, lash_kernel_value::operations::ContractId,
    lash_kernel_value::package::*,
};
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LinkRefusal {
    #[error(transparent)]
    SignatureRequired(Box<lash_kernel_doc::SignatureRequired>),
    #[error("Import {slot:?} ({name:?}) has no member")]
    MissingImport {
        slot: ImportSlot,
        name: QualifiedName,
    },
    #[error("Import {slot:?} ({name:?}) has the wrong kind")]
    KindMismatch {
        slot: ImportSlot,
        name: QualifiedName,
    },
    #[error("Import {slot:?} ({name:?}) has no serving signature")]
    SignatureMismatch {
        slot: ImportSlot,
        name: QualifiedName,
        mismatch: Option<Box<lash_kernel_value::SignatureMismatch>>,
    },
    #[error("Import {slot:?} ({name:?}) has another calling convention")]
    ConventionMismatch {
        slot: ImportSlot,
        name: QualifiedName,
    },
    #[error("member {name:?} is not visible")]
    NotVisible { name: QualifiedName },
    #[error("package path {path} conflicts")]
    PathConflict { path: PackagePath },
    #[error("publisher does not own {path}")]
    NotOwner { path: PackagePath },
    #[error("deferred path {path} is not attached")]
    UndeclaredDeferred { path: PackagePath },
    #[error("operation {use_:?} pins {pinned}, offered {offered}")]
    ContractMismatch {
        use_: OperationUse,
        pinned: ContractId,
        offered: ContractId,
    },
    #[error("invalid linked content: {message}")]
    InvalidDocument { message: String },
    #[error("missing Document {document}")]
    MissingDocument { document: DocumentId },
    #[error("missing Link {link}")]
    MissingLink { link: LinkId },
    #[error("the Document has no main")]
    MissingMain,
    #[error("Export {export:?} is not a function")]
    NotFunction { export: ExportName },
    #[error("Export {export:?} has non-data parameters or settled result")]
    NotDataOnly { export: ExportName },
}
impl From<lash_kernel_check::StartRefusal> for LinkRefusal {
    fn from(error: lash_kernel_check::StartRefusal) -> Self {
        match error {
            lash_kernel_check::StartRefusal::SignatureRequired(source) => {
                Self::SignatureRequired(source)
            }
            lash_kernel_check::StartRefusal::MissingMain => Self::MissingMain,
            lash_kernel_check::StartRefusal::NotFunction { export } => Self::NotFunction { export },
            lash_kernel_check::StartRefusal::NotDataOnly { export } => Self::NotDataOnly { export },
        }
    }
}
