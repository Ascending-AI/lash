//! The dialect's methods: the table of those it has, and a method call,
//! which a package member answers first where a package value may reach
//! its receiver (§2.4 of the host-package design).

use lash_kernel_doc::{Member, Place};

use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::Ranged;

use super::calls::{Given, absent, arguments_error, keyword_names};
use super::{Lowerer, Lowering, Operand};
use crate::diagnostics::{self, Code};
use crate::scope::Ty;
use lash_ext_py_helpers::methods::{METHODS, Method, method_names};

impl Lowerer<'_> {
    /// `receiver.name(...)`. Where a package value may reach the receiver,
    /// a package member answers before the dialect's method (§2.4).
    /// `awaited` is whether an `await` stands before the call, which the
    /// call then awaits in its own lowering.
    pub(super) fn method(
        &mut self,
        attribute: &ast::ExprAttribute,
        call: &ast::ExprCall,
        awaited: bool,
    ) -> Lowering<Operand> {
        if call.arguments.args.iter().any(PyExpr::is_starred_expr)
            || call
                .arguments
                .keywords
                .iter()
                .any(|keyword| keyword.arg.is_none())
        {
            return self.call_function(call, awaited);
        }
        let name = attribute.attr.id.as_str();
        let method = METHODS.iter().find(|method| method.name == name);
        // The callable's metadata owns keyword-only validation, including
        // aliases and unpacked calls, and raises errors at the call boundary.
        if method.is_some_and(|method| method.positional < method.max) {
            return self.call_function(call, awaited);
        }
        if self.seals() && !self.is_asyncio(&attribute.value) {
            return self.package_method(attribute, call, method, awaited);
        }
        let Some(method) = method else {
            return self.call_function(call, awaited);
        };
        let Given {
            head: receiver,
            positional,
            keywords: keyword_values,
        } = self.given(Some(&attribute.value), &call.arguments)?;
        let keywords = keyword_names(&call.arguments)
            .zip(keyword_values)
            .collect::<Vec<_>>();
        let slots = self.method_slots(method, receiver, positional, &keywords, call)?;
        self.ordinary_method(method, slots, attribute, awaited)
    }

    /// A method call where a package value may reach the receiver: the
    /// receiver's package type's member, the dialect's method on a
    /// receiver of another known type, or a selection between them.
    fn package_method(
        &mut self,
        attribute: &ast::ExprAttribute,
        call: &ast::ExprCall,
        method: Option<&'static Method>,
        awaited: bool,
    ) -> Lowering<Operand> {
        let name = attribute.attr.id.as_str();
        let receiver = self.expr(&attribute.value)?;
        if let Some(brand) = self.brand_of(receiver.ty) {
            return self.call_typed_member(receiver, &brand, name, call, awaited);
        }
        let targets = if receiver.ty == Ty::Unknown {
            self.selectable(name, true)
        } else {
            Vec::new()
        };
        if targets.is_empty() && method.is_none() {
            return Err(self.method_refusal(name, call));
        }
        if receiver.ty != Ty::Unknown
            && let Some(method) = method
        {
            self.no_stars(&call.arguments)?;
            let passed = self.passed(Some(receiver), &call.arguments)?;
            let receiver = passed.receiver.unwrap_or_else(Operand::none);
            let slots =
                self.method_slots(method, receiver, passed.positional, &passed.keywords, call)?;
            return self.ordinary_method(method, slots, attribute, awaited);
        }
        self.select_call(receiver, name, method, &targets, call, awaited)
    }

    /// The refusal of a method the dialect does not have.
    pub(super) fn method_refusal(
        &self,
        name: &str,
        call: &ast::ExprCall,
    ) -> lash_kernel_dialect::Diagnostic {
        let (range, receiver) = match call.func.as_ref() {
            PyExpr::Attribute(attribute) => (attribute.range, attribute.value.range()),
            other => (other.range(), other.range()),
        };
        let repair = format!(
            "replace `{}` with a supported method of `{}`; the dialect's methods are: {}",
            self.text(range),
            self.text(receiver),
            method_names().join(", ")
        );
        diagnostics::with_repair(
            Code::MethodUnsupported,
            format!("the method `{name}` is not in the dialect"),
            range,
            &repair,
        )
    }

    /// The slots of a call of the dialect's method `method`: the receiver,
    /// then each argument at its position or under its keyword, absent
    /// for one the call leaves out.
    pub(super) fn method_slots(
        &self,
        method: &Method,
        receiver: Operand,
        positional: Vec<Operand>,
        keywords: &[(String, Operand)],
        call: &ast::ExprCall,
    ) -> Lowering<Vec<Operand>> {
        let name = method.name;
        if method.packed {
            let fields = keywords
                .iter()
                .map(|(name, value)| lash_kernel_doc::MapEntry {
                    key: Operand::text(name).expr,
                    value: value.expr.clone(),
                })
                .collect();
            return Ok(vec![
                receiver,
                Self::tuple_of(&positional),
                Operand::inline(lash_kernel_doc::Expr::Map(fields), Ty::Dict),
            ]);
        }
        let mut slots: Vec<Option<Operand>> = positional.into_iter().map(Some).collect();
        for ((keyword_name, value), keyword) in keywords.iter().zip(&call.arguments.keywords) {
            let Some(index) = method
                .keywords
                .iter()
                .position(|known| known == keyword_name)
            else {
                return Err(arguments_error(
                    format!("`{name}` takes no keyword argument `{keyword_name}`"),
                    keyword.range,
                ));
            };
            if slots.len() <= index {
                slots.resize(index + 1, None);
            }
            if slots[index].is_some() {
                return Err(arguments_error(
                    format!("`{name}` got multiple values for `{keyword_name}`"),
                    keyword.range,
                ));
            }
            slots[index] = Some(value.clone());
        }
        if slots.len() < method.min || slots.len() > method.max {
            return Err(arguments_error(
                format!(
                    "`{name}` takes {} to {} arguments here, and {} were given",
                    method.min,
                    method.max,
                    slots.len()
                ),
                call.range(),
            ));
        }
        let mut all = vec![receiver];
        all.extend(slots.into_iter().map(|slot| slot.unwrap_or_else(absent)));
        Ok(all)
    }

    /// A call of the dialect's method over its slots, the receiver first.
    fn ordinary_method(
        &mut self,
        method: &Method,
        mut slots: Vec<Operand>,
        attribute: &ast::ExprAttribute,
        awaited: bool,
    ) -> Lowering<Operand> {
        let name = method.name;
        let list_local = matches!(attribute.value.as_ref(), PyExpr::Name(local)
            if self.variable(local.id.as_str()).is_some_and(|found|
                self.local_type(found.scope, local.id.as_str()) == Ty::List));
        if name == "append" && slots[0].ty == Ty::List && list_local {
            let value = slots.remove(1);
            let target = slots.remove(0).expr;
            let end = self.native("list.len", vec![target.clone()])?;
            // As in comprehension collection, evaluate the argument before
            // reading the end: it may itself mutate the receiver or park.
            self.store(Place::Member(Member::Index { target, index: end }), value);
            return Ok(Operand::none());
        }
        let value = self.invoke(
            &crate::constructs::access::method_name(name),
            &slots,
            method.result.into(),
        )?;
        if awaited {
            return self.await_via("py.await", value);
        }
        Ok(value)
    }
}
