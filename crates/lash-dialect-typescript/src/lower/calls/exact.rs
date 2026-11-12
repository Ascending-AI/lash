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
//!   them. The printer uses it for every row of the list's own table
//!   (`push`, `indexOf`, `sort`, ...). A receiver that is no list gets that
//!   row: its kind check where the row has one (`push` reads the list's
//!   length), else the generic row (`indexOf` of a text finds nothing).
//! - `record.method(r, "name", ...args)` is that call on a record the
//!   source built: its own field by that name where it is unbranded and
//!   holds one, else the built-in row's helper read, then applied to `r`,
//!   its receiver and named arguments read as `callable.method` reads them.
//!   A receiver that is no record gets the same ladder: a branded or
//!   fieldless value takes the helper's read, which fails where it runs.
//! - `value.method(t, "name", ...args)` is that call on a text the source
//!   built: the text's own row (`charAt`, `trim`, ...) as a text binding
//!   reads it, the receiver's check first, its receiver and named arguments read as `callable.method`
//!   reads them. A receiver that is no text gets the text row, which fails
//!   its own kind check where it runs.
//! - `list.length(xs)` is `xs.length` of a list the source built: its
//!   length as a float, with no kind test.
//! - `record.field(r, "name")` is `r.name` of a record the source built:
//!   its own field where it is unbranded, else the helper read.
//! - `list.field(xs, "name")` and `value.field(t, "name")` are `xs.name`
//!   of a list and `t.name` of a text the source built: the kind's own
//!   member read (`size` reads its helper), without the record ladder an
//!   unknown receiver takes. A receiver of another kind fails that read's
//!   own check where it runs.
//! - `value.length(t)` is `t.length` of a text the source built: the
//!   length helper's read, a number. (`text` is no reserved root.)
//! - `list.set(xs, i, v)` is the element write `xs[i] = v` of a list the
//!   source built: `i`'s position test and the bound that admits an
//!   append, then the element written in place, else `ts.set`. A receiver
//!   that is no list fails that bound's length read where the write runs.
//! - `value.initialized(k)` is the read of a `let` or `const` an enclosing
//!   function declares later: its temporal-dead-zone check (`ts.tdz`),
//!   which fails where the read runs while the binding is uninitialized.
//! - `value.operand(k)` is the binding `k` as an operator's operand, read
//!   where that operator reads it, as for a binding nothing reassigns,
//!   rather than held before its later operands run: `"a" in
//!   value.operand(k)`, `delete value.operand(k)[f]`, `new
//!   (value.operand(k))(...)`, `for (const p in value.operand(k))`,
//!   `value.operand(k)?.(...)`. The operator itself, its `?.` test and
//!   branch included, stays the source's, so an operand the original skips
//!   is skipped.
//! - `list.operand(xs)`, `record.operand(r)` and `value.text_operand(t)` are
//!   that read of a list, a record or a text the source built: the operator
//!   applied to it takes that kind's lowering (`list.operand(xs)?.[i]` the
//!   list's element read without a kind test, `record.operand(r)?.name` the
//!   record's own-field ladder, `value.text_operand(t)?.[i]` the text's
//!   index read, `const {a} = list.operand(xs)` the kind's field reads).
//!   None checks its operand's kind: a value of another kind fails where
//!   that kind's operation fails (`collection has the wrong kind`), or gets
//!   the generic helper the kind's lowering calls (a list's `ts.read`, a
//!   text's `ts.text_index`).
//!
//! - `value.text_join([p, ...], separator)` is the join a template literal
//!   lowers to, `text.join` of exactly those parts and that separator: each
//!   part evaluated in order and joined as it is, with no `ts.to_string`
//!   conversion added (a conversion already among the parts stays). A part
//!   that is no text fails the join's own check where it runs.
//!
//! None infers, pins, forwards or converts anything else: each lowers to
//! the nodes, helpers and order of the ordinary lowering it names, and keeps
//! that operation's kind and bounds failures at its own site. Their roots
//! are value-constructor roots, so namespace admission already reserves
//! them.

use super::{Key, Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;
use crate::{Diagnostic, DiagnosticCode, SourceSpan};
use lash_kernel_doc::Expr;

#[derive(Clone, Copy)]
pub(in crate::lower) enum Exact {
    ListElement,
    ListField,
    ListLength,
    ListMethod,
    ListSet,
    Method,
    RecordField,
    RecordMethod,
    TextField,
    TextLength,
    TextMethod,
    Initialized,
    Operand,
    ListOperand,
    RecordOperand,
    TextOperand,
    TextJoin,
}

impl Exact {
    pub(in crate::lower) fn named(path: &str) -> Option<Self> {
        let exact = match path {
            "list.element" => Self::ListElement,
            "list.field" => Self::ListField,
            "value.field" => Self::TextField,
            "list.length" => Self::ListLength,
            "list.method" => Self::ListMethod,
            "list.set" => Self::ListSet,
            "value.initialized" => Self::Initialized,
            "callable.method" => Self::Method,
            "record.field" => Self::RecordField,
            "record.method" => Self::RecordMethod,
            "value.length" => Self::TextLength,
            "value.method" => Self::TextMethod,
            "value.operand" => Self::Operand,
            "list.operand" => Self::ListOperand,
            "record.operand" => Self::RecordOperand,
            "value.text_operand" => Self::TextOperand,
            "value.text_join" => Self::TextJoin,
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
            Exact::Method | Exact::ListMethod | Exact::RecordMethod | Exact::TextMethod => {
                let (name, ty) = match exact {
                    Exact::Method => ("callable.method", Ty::Unknown),
                    Exact::RecordMethod => ("record.method", Ty::Object),
                    Exact::TextMethod => ("value.method", Ty::Text),
                    _ => ("list.method", Ty::Array),
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
                            ty,
                            ..self.pin(object)
                        };
                        // A text the source built is of its binding's
                        // type, not proven: its row checks the receiver.
                        let proven = !matches!(exact, Exact::TextMethod);
                        self.call_method_on(
                            object,
                            proven,
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
            Exact::ListSet => {
                let [
                    ast::CallArg::Value(object @ ast::Expr::Ident(receiver, _)),
                    ast::CallArg::Value(index),
                    ast::CallArg::Value(value),
                ] = args
                else {
                    return refuse("list.set takes a bound list, an index and a value");
                };
                if !self.is_bound(receiver) {
                    return refuse("list.set takes a bound list");
                }
                // `lower_assign`'s order: the receiver, read in place as a
                // binding nothing reassigns is, then the key, then the
                // value, each held before the write.
                let added = self.in_place.insert(receiver.clone());
                let object = self.lower_expr(object).map(|object| self.pin(object));
                if added {
                    self.in_place.remove(receiver);
                }
                let object = Operand {
                    ty: Ty::Array,
                    ..object?
                };
                let property = ast::MemberProperty::Index(Box::new(index.clone()));
                let key = match self.lower_key(&property)? {
                    Key::Computed(key) => Key::Computed(self.pin(key)),
                    Key::Static(_) => return refuse("list.set takes a computed index"),
                };
                let value = self.lower_expr(value)?;
                let value = self.pin(value);
                self.set_member(&object, &key, value)?;
                Ok(Operand::undefined())
            }
            Exact::Initialized => {
                let [ast::CallArg::Value(ast::Expr::Ident(name, _))] = args else {
                    return refuse("value.initialized takes a binding");
                };
                let Some(kernel) = self.binding_kernel(name) else {
                    return refuse("value.initialized takes a binding");
                };
                let read = match self.resolve(name, Some(span))? {
                    // The read is checked already where the binding is not
                    // yet declared.
                    Some(checked) if checked != kernel => checked,
                    _ => self.initialised(kernel, name)?,
                };
                Ok(Operand::variable(read, Ty::Unknown))
            }
            Exact::Operand | Exact::ListOperand | Exact::RecordOperand | Exact::TextOperand => {
                let [ast::CallArg::Value(ast::Expr::Ident(name, _))] = args else {
                    return refuse("an operand spelling takes a binding");
                };
                if !self.is_bound(name) {
                    return refuse("an operand spelling takes a binding");
                }
                // The binding's ordinary read, its checks included, read
                // again wherever the operator applied to it reads it.
                let read = self.read(name, Some(span))?;
                let ty = match exact {
                    Exact::ListOperand => Ty::Array,
                    Exact::RecordOperand => Ty::Object,
                    Exact::TextOperand => Ty::Text,
                    _ => {
                        return Ok(Operand {
                            stable: true,
                            ..read
                        });
                    }
                };
                Ok(Operand {
                    ty,
                    stable: true,
                    plain: None,
                    text_prefix: None,
                    ..read
                })
            }
            Exact::TextJoin => {
                let [
                    ast::CallArg::Value(ast::Expr::Array(elements)),
                    ast::CallArg::Value(separator),
                ] = args
                else {
                    return refuse("value.text_join takes a list of parts and a separator");
                };
                // A template's order: each part where the source evaluates
                // it, then the separator, held only before later statements.
                let mut parts = Vec::with_capacity(elements.len() + 1);
                for element in elements {
                    let ast::ArrayElement::Value(part) = element else {
                        return refuse("value.text_join takes its parts one by one");
                    };
                    parts.push(self.aside(|this| this.lower_expr(part))?);
                }
                parts.push(self.aside(|this| this.lower_expr(separator))?);
                let mut parts = self.ordered(parts);
                let Some(separator) = parts.pop() else {
                    return refuse("value.text_join takes a separator");
                };
                let parts = self.inline(
                    Expr::List(parts.iter().map(Operand::expr).collect()),
                    Ty::Unknown,
                );
                let joined = self.native("text.join", vec![parts.expr(), separator.expr()])?;
                Ok(self.inline(joined, Ty::Text))
            }
            Exact::ListLength
            | Exact::ListField
            | Exact::RecordField
            | Exact::TextField
            | Exact::TextLength => {
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
                    (
                        Exact::ListField | Exact::TextField,
                        [
                            ast::CallArg::Value(object),
                            ast::CallArg::Value(ast::Expr::String(name)),
                        ],
                    ) => (
                        object,
                        name.clone(),
                        if matches!(exact, Exact::ListField) {
                            Ty::Array
                        } else {
                            Ty::Text
                        },
                    ),
                    (Exact::RecordField | Exact::ListField | Exact::TextField, _) => {
                        return refuse("a field read takes its receiver and a field name");
                    }
                    _ => return refuse("a length read takes its receiver alone"),
                };
                let object = self.lower_expr(object)?;
                self.get_member(&Operand { ty, ..object }, &Key::Static(key))
            }
        }
    }
}
