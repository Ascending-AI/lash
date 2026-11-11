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
//! - `list.method(xs, "name", ...args)` is that call on a list the source
//!   built: the list's own row (`map` over its slots, say) without the kind
//!   test, its receiver and named arguments read as `callable.method` reads
//!   them. A receiver that is no list fails that row's kind check.
//! - `list.length(xs)` is `xs.length` of a list the source built: its
//!   length as a float, with no kind test.
//! - `record.field(r, "name")` is `r.name` of a record the source built:
//!   its own field where it is unbranded, else the helper read.
//! - `value.length(t)` is `t.length` of a text the source built: the
//!   length helper's read, a number. (`text` is no reserved root.)
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
    ListLength,
    ListMethod,
    Method,
    RecordField,
    TextLength,
}

impl Exact {
    pub(in crate::lower) fn named(path: &str) -> Option<Self> {
        let exact = match path {
            "list.element" => Self::ListElement,
            "list.length" => Self::ListLength,
            "list.method" => Self::ListMethod,
            "callable.method" => Self::Method,
            "record.field" => Self::RecordField,
            "value.length" => Self::TextLength,
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
            Exact::Method | Exact::ListMethod => {
                let name = match exact {
                    Exact::Method => "callable.method",
                    _ => "list.method",
                };
                let [
                    ast::CallArg::Value(object @ ast::Expr::Ident(..)),
                    ast::CallArg::Value(ast::Expr::String(method)),
                    rest @ ..,
                ] = args
                else {
                    return refuse(&format!(
                        "{name} takes a bound receiver, a method name and its arguments"
                    ));
                };
                let ast::Expr::Ident(receiver, _) = object else {
                    unreachable!("matched an identifier");
                };
                if !self.is_bound(receiver) {
                    return refuse(&format!("{name} takes a bound receiver"));
                }
                let named: Vec<String> = std::iter::once(receiver.clone())
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
                let property = ast::MemberProperty::Field(method.clone());
                let called = match exact {
                    Exact::Method => {
                        self.call_method(object, &property, rest, span, discard_result, awaited)
                    }
                    _ => self.lower_expr(object).and_then(|object| {
                        let object = Operand {
                            ty: Ty::Array,
                            ..self.pin(object)
                        };
                        self.call_method_on(
                            object,
                            true,
                            super::MethodCall {
                                property: &property,
                                args: rest,
                                span,
                                discard_result,
                                awaited,
                            },
                        )
                    }),
                };
                for name in added {
                    self.in_place.remove(&name);
                }
                called
            }
            Exact::ListLength | Exact::RecordField | Exact::TextLength => {
                let (object, key, ty) = match (exact, args) {
                    (Exact::ListLength, [ast::CallArg::Value(object)]) => {
                        (object, "length".to_owned(), Ty::Array)
                    }
                    (Exact::TextLength, [ast::CallArg::Value(object)]) => {
                        (object, "length".to_owned(), Ty::Text)
                    }
                    (
                        Exact::RecordField,
                        [
                            ast::CallArg::Value(object),
                            ast::CallArg::Value(ast::Expr::String(name)),
                        ],
                    ) => (object, name.clone(), Ty::Object),
                    (Exact::RecordField, _) => {
                        return refuse("record.field takes a record and a field name");
                    }
                    _ => return refuse("a length read takes its receiver alone"),
                };
                let object = self.lower_expr(object)?;
                self.get_member(&Operand { ty, ..object }, &Key::Static(key))
            }
        }
    }
}
