pub const REPLY: &str = "py.reply";
use lash_kernel_dialect::conventions::{Boundary, Conventions, Iteration};
pub const ITERATIONS: &[(Option<&str>, Iteration)] = &[
    (
        None,
        Iteration {
            open: "py.for_in.open",
            next: Some("py.iterator.step"),
            item: None,
            check: Some("py.for_in.check"),
        },
    ),
    (
        Some("keys"),
        Iteration {
            open: "py.for_in.keys",
            next: None,
            item: None,
            check: Some("py.for_in.check"),
        },
    ),
    (
        Some("values"),
        Iteration {
            open: "py.for_in.values",
            next: None,
            item: Some("py.for_in.value"),
            check: Some("py.for_in.check"),
        },
    ),
    (
        Some("items"),
        Iteration {
            open: "py.for_in.items",
            next: None,
            item: Some("py.for_in.item"),
            check: Some("py.for_in.check"),
        },
    ),
];
pub const CONVENTIONS: Conventions = Conventions {
    language: Some(lash_kernel_doc::SourceLanguage::Python),
    matches_start: |start| {
        matches!(
            start,
            lash_kernel_doc::StartConvention::PyDirect { .. }
                | lash_kernel_doc::StartConvention::PyArgsList { .. }
        )
    },
    helper_prefix: "py.",
    iteration: Some(ITERATIONS[0].1),
    argument: Boundary::Identity,
    reply: Boundary::Call {
        role: Some(lash_kernel_doc::ProjectionRole::ResultDecode),
        helper: REPLY,
        tail: &[],
        literals_passthrough: false,
    },
    then_yield: false,
};
pub fn iteration(view: Option<&str>) -> Option<&'static Iteration> {
    ITERATIONS
        .iter()
        .find(|(name, _)| *name == view)
        .map(|(_, iteration)| iteration)
}
