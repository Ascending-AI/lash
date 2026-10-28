pub const ARGUMENT_COPY: &str = "ts.copy_boundary";
use lash_kernel_dialect::conventions::{Boundary, BoundaryOperand, Conventions, Iteration};
pub const FOR_OF: Iteration = Iteration {
    open: "ts.for_of.open",
    next: Some("ts.for_of.next"),
    item: Some("ts.for_of.item"),
    check: None,
};
pub const CONVENTIONS: Conventions = Conventions {
    language: Some(lash_kernel_doc::SourceLanguage::TypeScript),
    matches_start: |start| {
        matches!(
            start,
            lash_kernel_doc::StartConvention::TsDirect { .. }
                | lash_kernel_doc::StartConvention::TsArgsList { .. }
        )
    },
    helper_prefix: "ts.",
    iteration: Some(FOR_OF),
    argument: Boundary::Call {
        role: Some(lash_kernel_doc::ProjectionRole::ArgumentCopy),
        helper: ARGUMENT_COPY,
        tail: &[BoundaryOperand::Absent],
        literals_passthrough: true,
    },
    reply: Boundary::Identity,
    then_yield: true,
};
