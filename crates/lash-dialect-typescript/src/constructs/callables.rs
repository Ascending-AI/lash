//! The authored function token is one bidirectional tuple recipe.
use lash_kernel_dialect::construct::{Atom, Expression as E};
use lash_kernel_doc::{self as k, Expr};
pub(crate) const TAG: &str = "ts.function";
pub(crate) const CALLABLE: u32 = 4;
const TOKEN: E = E::Tuple(&[
    E::Atom(Atom::Text(TAG)),
    E::Input(0),
    E::Input(1),
    E::Input(2),
    E::Input(3),
    E::Input(4),
]);

pub(crate) struct Token<'a> {
    pub path: &'a str,
    pub length: &'a k::Float,
    pub closure: &'a k::Closure,
    pub properties: &'a Expr,
}
pub(crate) fn token(closure: Expr, name: &str, length: k::Float) -> Expr {
    TOKEN
        .emit(
            &[
                Expr::Literal(k::Literal::Text(String::new())),
                Expr::Literal(k::Literal::Text(name.into())),
                Expr::Literal(k::Literal::Float(length)),
                closure,
                Expr::Record(Vec::new()),
            ],
            &mut |_, _| -> Result<Expr, std::convert::Infallible> {
                unreachable!("the token contains no calls")
            },
        )
        .unwrap_or_else(|never| match never {})
}
pub(crate) fn decode(value: &Expr) -> Option<Token<'_>> {
    let values = TOKEN.recognize(value, &|_, _| false)?;
    let [
        Expr::Literal(k::Literal::Text(path)),
        Expr::Literal(k::Literal::Text(_)),
        Expr::Literal(k::Literal::Float(length)),
        Expr::Closure(closure),
        properties,
    ] = values.as_slice()
    else {
        return None;
    };
    Some(Token {
        path,
        length,
        closure,
        properties,
    })
}
pub(crate) fn authored_name(value: &Expr) -> Option<&str> {
    let values = TOKEN.recognize(value, &|_, _| false)?;
    let [
        _,
        Expr::Literal(k::Literal::Text(name)),
        _,
        Expr::Closure(_),
        _,
    ] = values.as_slice()
    else {
        return None;
    };
    Some(name)
}
