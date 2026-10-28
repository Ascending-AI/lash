//! Python callable token construction and structural admission share one layout.
use lash_kernel_doc::{self as k, Expr};
pub(crate) const TAG: &str = "py.function";
pub(crate) const CALLABLE: usize = 1;
pub(crate) const PARAMETERS: usize = 2;
pub(crate) const NAME: usize = 3;
pub(crate) const CATEGORY: usize = 4;
pub(crate) const SIZE: usize = 5;
pub(crate) fn token(closure: Expr, metadata: Vec<Expr>, name: &str) -> Expr {
    Expr::Tuple(vec![
        Expr::Literal(k::Literal::Text(TAG.into())),
        closure,
        Expr::Tuple(metadata),
        Expr::Literal(k::Literal::Text(name.into())),
        Expr::Literal(k::Literal::Null),
    ])
}
pub(crate) fn parts(value: &Expr) -> Option<&[Expr]> {
    let Expr::Tuple(parts) = value else {
        return None;
    };
    (parts.len() == SIZE && matches!(&parts[0], Expr::Literal(k::Literal::Text(tag)) if tag == TAG))
        .then_some(parts)
}
