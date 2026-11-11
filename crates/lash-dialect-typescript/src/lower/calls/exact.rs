//! Exact operation spellings (FIG-7697).
//!
//! An ordinary read or method call takes its lowering from what the front
//! end knows about the receiver binding: a list the source built reads its
//! element without a kind test, and a binding nothing reassigns is read
//! where the call reads it rather than held first. A materialized capture
//! cell is a reassigned binding of no known kind, so its ordinary spelling
//! can lower differently. These spellings name the operation itself:
//!
//! - `list.element(xs, i)` is the element read `xs[i]` of a list the source
//!   built: the list's bounds test, its element (a hole is `undefined`), and
//!   otherwise the dialect's numeric read. A receiver that is no list fails
//!   that bounds test's kind check where the read runs.
//! - `callable.method(receiver, "name", ...args)` is `receiver.name(args)`
//!   with the bound receiver, and each argument that names a binding, read
//!   where the method's dispatch reads them, as for bindings nothing
//!   reassigns, instead of held before the later operands run.
//!
//! Neither infers, pins, forwards or converts anything else: each lowers to
//! the nodes, helpers and order of the ordinary lowering it names, and keeps
//! that operation's kind and bounds failures at its own site. Their roots
//! are value-constructor roots, so namespace admission already reserves
//! them.

use super::{Key, Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

#[derive(Clone, Copy)]
pub(in crate::lower) enum Exact {
    ListElement,
    Method,
}

impl Exact {
    pub(in crate::lower) fn named(path: &str) -> Option<Self> {
        let exact = match path {
            "list.element" => Self::ListElement,
            "callable.method" => Self::Method,
            _ => return None,
        };
        debug_assert!(path.split_once('.').is_some_and(|(root, _)| {
            lash_kernel_dialect::value_constructors::Constructor::reserves(root)
        }));
        Some(exact)
    }
}

impl Lowerer<'_> {
    pub(super) fn call_exact(
        &mut self,
        exact: Exact,
        args: &[ast::CallArg],
        span: SourceSpan,
        discard_result: bool,
        awaited: bool,
    ) -> Lowering<Operand> {
        let refuse = |message: &str| {
            Err(Diagnostic::new(
                DiagnosticCode::InvalidAst,
                message.to_owned(),
                Some(span),
            ))
        };
        match exact {
            Exact::ListElement => {
                let [ast::CallArg::Value(object), ast::CallArg::Value(index)] = args else {
                    return refuse("list.element takes a list and an index");
                };
                // `lower_member`'s order: the receiver, then the key, holding
                // the receiver only when the key runs statements.
                let object = self.lower_expr(object)?;
                let property = ast::MemberProperty::Index(Box::new(index.clone()));
                let (buf, key) = self.aside(|this| this.lower_key(&property))?;
                let object = if buf.is_empty() {
                    object
                } else {
                    self.pin(object)
                };
                self.append(buf);
                if !matches!(key, Key::Computed(_)) {
                    return refuse("list.element takes a computed index");
                }
                let object = Operand {
                    ty: Ty::Array,
                    ..object
                };
                self.get_member(&object, &key)
            }
            Exact::Method => {
                let [
                    ast::CallArg::Value(object @ ast::Expr::Ident(..)),
                    ast::CallArg::Value(ast::Expr::String(method)),
                    rest @ ..,
                ] = args
                else {
                    return refuse(
                        "callable.method takes a bound receiver, a method name and its arguments",
                    );
                };
                let ast::Expr::Ident(name, _) = object else {
                    unreachable!("matched an identifier");
                };
                if !self.is_bound(name) {
                    return refuse("callable.method takes a bound receiver");
                }
                let named: Vec<String> = std::iter::once(name.clone())
                    .chain(rest.iter().filter_map(|arg| match arg {
                        ast::CallArg::Value(ast::Expr::Ident(name, _)) if self.is_bound(name) => {
                            Some(name.clone())
                        }
                        _ => None,
                    }))
                    .collect();
                let added: Vec<String> = named
                    .into_iter()
                    .filter(|name| self.in_place.insert(name.clone()))
                    .collect();
                let called = self.call_method(
                    object,
                    &ast::MemberProperty::Field(method.clone()),
                    rest,
                    span,
                    discard_result,
                    awaited,
                );
                for name in added {
                    self.in_place.remove(&name);
                }
                called
            }
        }
    }
}
